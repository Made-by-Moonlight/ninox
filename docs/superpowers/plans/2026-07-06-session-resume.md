# Resuming Interrupted Sessions After a Reboot — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When ninox's private tmux server dies in a reboot (taking every session pane with it), let the user resume the exact same Claude conversation with one click instead of losing the ledger row to `Terminated`.

**Architecture:** ninox assigns each `claude-code` session its own UUID at spawn time via `--session-id`, persists it on the session's DB row, and on startup distinguishes "process died along with the tmux server" (new `SessionStatus::Interrupted`, resumable) from "process died on its own" or "harness can't resume" (`Terminated`, unchanged). A user-triggered Resume relaunches `claude --resume <uuid>` in the session's original workspace.

**Tech Stack:** Rust, `rusqlite` (SQLite), `iced` (GUI), `tmux` (private server, socket `-L ninox`), `uuid` crate (promoted from transitive to direct dependency).

## Global Constraints

- Full design context lives in `docs/superpowers/specs/2026-07-06-session-resume-design.md` — read it if anything below is ambiguous.
- No automatic/silent respawn. Every resume is user-initiated (per-session or bulk "Resume all").
- No `--continue`, no tmux-resurrect/continuum. Resume is always by explicit `--session-id`/`--resume <uuid>`, generated and owned by ninox.
- Only `claude-code`'s builtin `HarnessSpec` gets `resume_args`. Every other harness (`codex`, `aider`, `opencode`, `freebuff`, unknown) keeps empty `resume_args` and therefore never shows a Resume affordance — this must remain true after every task below.
- **Every existing `matches!(status, SessionStatus::Done | SessionStatus::Terminated)` terminal-state guard must add `Interrupted` to the pattern.** There are five: `lifecycle/poller.rs`'s `poll_pids`, `sync_sessions_metadata`, `poll_usage`, `poll_github`, plus `app.rs`'s startup reconciliation task. Missing any one reintroduces the bug where `poll_pids` re-terminates a freshly-`Interrupted` session within 5 seconds because its stored `pid` is stale/dead.
- This repo is TDD: every task writes a failing test before the implementation that makes it pass.
- All work happens in the existing worktree `.claude/worktrees/feat-resume-interrupted-sessions` on branch `feat/resume-interrupted-sessions`. Do not create a new worktree. Run all commands from that worktree's root unless a step says otherwise.
- Run `cargo test --workspace` (not just the touched crate) at the end of every task — this feature's changes ripple `Session`-literal compile errors across crates (`ninox-core`, `ninox-app`, `ninox-server`).

---

### Task 1: Data model — `claude_session_id` column and `Session` field

**Files:**
- Modify: `crates/ninox-core/src/types.rs` (`Session` struct)
- Modify: `crates/ninox-core/src/store.rs` (schema, migration, `upsert_session`, `list_sessions`, `get_session`)
- Modify: `crates/ninox-app/src/app.rs:984` (Standalone spawn `Session` literal), `crates/ninox-app/src/app.rs:1118` (Orchestrator spawn `Session` literal)
- Modify: `crates/ninox-app/src/main.rs` (`run_spawn`'s `Session` literal, ~line 205)
- Modify: `crates/ninox-app/src/spawn_util.rs` (`spawn_interactive_session`'s `Session` literal, ~line 103)
- Test: `crates/ninox-core/src/store.rs` (new test in `mod tests`)

**Interfaces:**
- Produces: `Session.claude_session_id: Option<String>` — every later task that reads/writes a session's resumable-conversation id uses this field, by this exact name.
- Produces: `Store::upsert_session`/`list_sessions`/`get_session` all round-trip `claude_session_id`.

- [ ] **Step 1: Write the failing store test**

Add to `crates/ninox-core/src/store.rs`'s `mod tests`, right after `catalogue_path_round_trips`:

```rust
    #[test]
    fn claude_session_id_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s3".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            claude_session_id: Some("b7e0b3a0-0000-4000-8000-000000000001".into()),
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s3").unwrap().unwrap();
        assert_eq!(found.claude_session_id.as_deref(), Some("b7e0b3a0-0000-4000-8000-000000000001"));
        // list path decodes it too
        assert_eq!(
            store.list_sessions().unwrap().iter().find(|x| x.id == "s3").unwrap().claude_session_id.as_deref(),
            Some("b7e0b3a0-0000-4000-8000-000000000001"),
        );

        // None round-trips as None, not "" or "none"
        let mut s2 = s.clone();
        s2.id = "s4".into();
        s2.claude_session_id = None;
        store.upsert_session(&s2).unwrap();
        assert_eq!(store.get_session("s4").unwrap().unwrap().claude_session_id, None);
    }
```

This won't compile yet (the `Session` literal has an extra field the struct doesn't have) — that's expected; it's what step 2 confirms.

- [ ] **Step 2: Confirm it fails to compile**

Run: `cargo test -p ninox-core --lib claude_session_id_round_trips 2>&1 | tail -20`
Expected: compile error, `struct Session has no field named claude_session_id` (or similar "no such field").

- [ ] **Step 3: Add the field to `Session`**

In `crates/ninox-core/src/types.rs`, in the `Session` struct, right after `catalogue_path`:

```rust
    #[serde(default)]
    pub catalogue_path: Option<String>,
    /// UUID ninox assigned this session's `claude` CLI process at spawn
    /// time (`--session-id <uuid>`), used to resume the exact same
    /// conversation later (`--resume <uuid>`) if the tmux pane dies
    /// out from under it (see `docs/superpowers/specs/2026-07-06-session-resume-design.md`).
    /// `None` for legacy sessions and for harnesses with no `resume_args`.
    #[serde(default)]
    pub claude_session_id: Option<String>,
}
```

(This replaces the existing closing `}` of the struct — the new field becomes the last one.)

- [ ] **Step 4: Add the column, migration, and CRUD wiring in `store.rs`**

In `crates/ninox-core/src/store.rs`, add the migration to the existing list (right after `catalogue_path`):

```rust
        for (col, ddl) in [
            ("model",             "ALTER TABLE sessions ADD COLUMN model TEXT"),
            ("context_tokens",    "ALTER TABLE sessions ADD COLUMN context_tokens INTEGER"),
            ("catalogue_path",    "ALTER TABLE sessions ADD COLUMN catalogue_path TEXT"),
            ("claude_session_id", "ALTER TABLE sessions ADD COLUMN claude_session_id TEXT"),
        ] {
```

Update `upsert_session`'s SQL and params (add `claude_session_id` as the 16th column):

```rust
    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,claude_session_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(id) DO UPDATE SET
             status=excluded.status,cost_usd=excluded.cost_usd,
             started_at=excluded.started_at,
             pr_number=excluded.pr_number,pr_id=excluded.pr_id,
             workspace_path=excluded.workspace_path,pid=excluded.pid,
             model=excluded.model,context_tokens=excluded.context_tokens,
             catalogue_path=excluded.catalogue_path,
             claude_session_id=excluded.claude_session_id",
            params![
                s.id, s.orchestrator_id, s.name, s.repo, status, s.agent_type,
                s.cost_usd, s.started_at, s.pr_number, s.pr_id,
                s.workspace_path, s.pid, s.model, s.context_tokens,
                s.catalogue_path, s.claude_session_id
            ],
        )?;
        Ok(())
    }
```

Update `list_sessions`'s `SELECT`, tuple type, `query_map` closure, destructuring, and `Session` construction — add `claude_session_id` as the 16th column throughout:

```rust
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,claude_session_id
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
                r.get::<_, Option<String>>(15)?,
            ))
        })?;
        rows.map(|r| {
            let (id, orchestrator_id, name, repo, status_str, agent_type,
                 cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                 model, context_tokens, catalogue_path, claude_session_id) = r?;
            let status = serde_json::from_str(&format!("\"{status_str}\""))
                .unwrap_or(SessionStatus::Working);
            Ok(Session {
                id, orchestrator_id, name, repo, status, agent_type,
                cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                catalogue_path, claude_session_id,
            })
        })
        .collect()
    }
```

`get_session` has the identical `SELECT ... FROM sessions WHERE id = ?1` shape (same 15-then-16 columns and the same tuple/destructure/construct pattern as `list_sessions`, just with a `WHERE` clause) — apply the exact same three edits (SELECT column list, tuple/query_map, destructure+construct) to it. Read the function first (it's directly below `list_sessions`) to confirm its exact current column order before editing, since a copy-paste-driven mismatch here silently reads the wrong column.

- [ ] **Step 5: Fix the two production `Session` literals in `app.rs`**

At `crates/ninox-app/src/app.rs:984` (Standalone spawn) and `:1118` (Orchestrator spawn), both currently end their `Session { ... }` literal with `catalogue_path: Some(catalogue_path.clone()),` (Standalone) / `catalogue_path: Some(catalogue_path.clone()),` (Orchestrator) followed by `};`. Add, right after that line in both literals:

```rust
                            claude_session_id: None, // set below, in Task 6
```

(Task 6 replaces this placeholder value with the real generated UUID once `interactive_cmd`'s new signature exists — for now this task's only job is making the struct compile with correct shape everywhere.)

- [ ] **Step 6: Fix the production `Session` literal in `main.rs`**

In `crates/ninox-app/src/main.rs`'s `run_spawn`, the `Session { ... }` literal ends with:
```rust
        catalogue_path:  std::env::var("NINOX_BRAIN").ok().filter(|s| !s.is_empty()),
    };
```
Add a field before the closing brace:
```rust
        catalogue_path:  std::env::var("NINOX_BRAIN").ok().filter(|s| !s.is_empty()),
        claude_session_id: None, // set in Task 7
    };
```

- [ ] **Step 7: Fix the production `Session` literal in `spawn_util.rs`**

In `crates/ninox-app/src/spawn_util.rs`'s `spawn_interactive_session`, the `Session { ... }` literal ends with:
```rust
        catalogue_path:  (!p.catalogue_path.is_empty()).then(|| p.catalogue_path.clone()),
    };
```
Add a field before the closing brace:
```rust
        catalogue_path:  (!p.catalogue_path.is_empty()).then(|| p.catalogue_path.clone()),
        claude_session_id: None, // set in Task 5
    };
```

- [ ] **Step 8: Compiler-driven fixup of every remaining `Session` literal**

Every other `Session { ... }` construction in the workspace is test-only (verified during planning: 39 total `catalogue_path:` sites across the workspace, of which exactly these 4 production ones plus test helpers in `crates/ninox-core/src/store.rs`, `events.rs`, `lifecycle/reactions.rs`, `lifecycle/poller.rs`, `crates/ninox-server/src/routes/sessions.rs`, and `crates/ninox-app/src/app.rs`'s test module — including the shared fixtures `pr_session` at `app.rs:2403` and `refile_session` at `app.rs:2533`). Fix them mechanically:

Run: `cargo build --workspace --tests 2>&1 | grep -B4 "missing field \`claude_session_id\`" | grep "\-\->"`

This prints every remaining file:line with a struct literal missing the field. For each, open the file and add `claude_session_id: None,` immediately after that literal's `catalogue_path:` field (matching the exact style already used at that call site — some are one-per-line, some are comma-packed on one line like `model: None, context_tokens: None, catalogue_path: None,` → becomes `model: None, context_tokens: None, catalogue_path: None, claude_session_id: None,`).

Repeat the build command until it reports zero `missing field` errors for `claude_session_id`.

- [ ] **Step 9: Run the new test and the full workspace suite**

Run: `cargo test -p ninox-core --lib claude_session_id_round_trips -- --nocapture`
Expected: PASS

Run: `cargo test --workspace`
Expected: all pass (this also validates every Step 8 fixup compiled and didn't break an existing assertion).

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat(core): add claude_session_id column for resumable sessions"
```

---

### Task 2: `SessionStatus::Interrupted` variant

**Files:**
- Modify: `crates/ninox-core/src/types.rs` (`SessionStatus` enum)
- Modify: `crates/ninox-app/src/style.rs` (`stamp_word`)
- Modify: `crates/ninox-app/src/theme.rs` (`ColorScheme::status_color`)
- Test: `crates/ninox-core/src/types.rs` (new `mod tests` — this file has none yet, so this step also creates it), `crates/ninox-app/src/style.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `SessionStatus::Interrupted` — Task 9 (reconciliation) sets it; Task 10 (resume action) reads it; Task 12 (fleet board) renders it.

- [ ] **Step 1: Write the failing serde round-trip test**

`crates/ninox-core/src/types.rs` currently has no `mod tests`. Add one at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_status_serializes_snake_case() {
        let json = serde_json::to_string(&SessionStatus::Interrupted).unwrap();
        assert_eq!(json, "\"interrupted\"");
        let back: SessionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SessionStatus::Interrupted);
    }
}
```

- [ ] **Step 2: Confirm it fails to compile**

Run: `cargo test -p ninox-core --lib interrupted_status_serializes_snake_case 2>&1 | tail -20`
Expected: `no variant named 'Interrupted' found for enum 'SessionStatus'`.

- [ ] **Step 3: Add the variant**

In `crates/ninox-core/src/types.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Spawning, Working, PrOpen, CiFailed,
    ReviewPending, Mergeable, Done, Terminated,
    /// Its tmux pane died along with the private tmux server (e.g. a
    /// reboot) rather than exiting on its own. Distinct from `Terminated`
    /// ("gone for good") — an `Interrupted` session has a
    /// `claude_session_id` and a harness capable of `--resume`, so the
    /// user can pick the exact same conversation back up. Never set
    /// silently: only the startup reconciliation in `app.rs` assigns it,
    /// and only a user-triggered Resume action clears it.
    Interrupted,
}
```

- [ ] **Step 4: Run the new test**

Run: `cargo test -p ninox-core --lib interrupted_status_serializes_snake_case`
Expected: PASS

- [ ] **Step 5: Fix `stamp_word`'s now-non-exhaustive match**

Run: `cargo build -p ninox-app 2>&1 | grep -A5 "non-exhaustive"`
Expected: it names `crate::style::stamp_word` (and `theme.rs`'s `status_color` — fixed in the next step).

In `crates/ninox-app/src/style.rs`:

```rust
pub fn stamp_word(status: &SessionStatus) -> &'static str {
    use SessionStatus::*;
    match status {
        Spawning | Working => "Working",
        PrOpen             => "PR Open",
        CiFailed           => "Failed",
        ReviewPending      => "Awaiting",
        Mergeable          => "Ready",
        Done               => "Filed",
        Terminated         => "Closed",
        Interrupted        => "Interrupted",
    }
}
```

Add a test next to the existing `stamp_word` assertions in `style.rs`'s `mod tests` (find the block containing `assert_eq!(stamp_word(&SessionStatus::Terminated), "Closed");` and add directly after it):

```rust
        assert_eq!(stamp_word(&SessionStatus::Interrupted),   "Interrupted");
```

- [ ] **Step 6: Fix `status_color`'s now-non-exhaustive match**

In `crates/ninox-app/src/theme.rs`:

```rust
    pub fn status_color(&self, status: &SessionStatus) -> Color {
        use SessionStatus::*;
        match status {
            Spawning | Working => self.status_working,
            PrOpen             => self.status_pr_open,
            CiFailed           => self.status_ci_failed,
            ReviewPending      => self.status_review,
            Mergeable          => self.status_mergeable,
            Done | Terminated  => self.status_done,
            Interrupted        => self.status_review,
        }
    }
```

`Interrupted` deliberately reuses `status_review` (the "needs your attention/action" color already used for `ReviewPending`) rather than adding a new `ColorScheme` field — both states mean "blocked on a human decision," and a new field would ripple into every theme preset file for no visual need this feature requires.

- [ ] **Step 7: Run the full workspace suite**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(core): add SessionStatus::Interrupted"
```

---

### Task 3: Harness registry — thread `claude_session_id` through `interactive_cmd`/`worker_cmd`

**Files:**
- Modify: `crates/ninox-core/src/harness.rs` (`expand_args`, `interactive_cmd`, `worker_cmd`, `claude-code` builtin spec, all existing tests that call either function)

**Interfaces:**
- Consumes: none new (this task only changes signatures/behavior within `harness.rs`).
- Produces: `HarnessRegistry::interactive_cmd(&self, agent: &AgentConfig, claude_session_id: &str) -> String` (was `(&self, agent)`); `HarnessRegistry::worker_cmd(&self, agent: &AgentConfig, prompt: &str, claude_session_id: &str) -> Option<String>` (was `(&self, agent, prompt)`). Every caller in `ninox-app` (fixed in Tasks 5–8) must pass a real UUID here.

- [ ] **Step 1: Write the failing tests for the new signatures and `--session-id`**

In `crates/ninox-core/src/harness.rs`'s `mod tests`, replace the two existing tests below with updated versions (same test names — this is a signature-and-behavior update, not new tests):

```rust
    #[test]
    fn interactive_cmd_claude_with_model() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", Some("claude-opus-4-5")), "sess-1"),
                   "claude --session-id 'sess-1' --model 'claude-opus-4-5'");
    }

    #[test]
    fn interactive_cmd_claude_without_model_is_bare() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", None), "sess-1"),
                   "claude --session-id 'sess-1'");
    }
```

```rust
    #[test]
    fn worker_cmd_claude_code() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", None), "Fix the bug", "sess-1").unwrap(),
                   "claude --dangerously-skip-permissions --session-id 'sess-1' -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_claude_code_with_model() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", Some("claude-opus-4-5")), "Fix the bug", "sess-1").unwrap(),
                   "claude --dangerously-skip-permissions --session-id 'sess-1' --model 'claude-opus-4-5' -- 'Fix the bug'");
    }
```

Every other test in this file that calls `.interactive_cmd(...)` or `.worker_cmd(...)` needs a third argument added (a session id, output unaffected since their harnesses' arg templates don't reference `{session_id}`) — update each call site in place:

- `worker_cmd_codex`: `reg().worker_cmd(&agent("codex", Some("gpt-4o")), "do the thing", "sess-1").unwrap()` (expected string unchanged).
- `worker_cmd_aider_uses_message_flag`: `reg().worker_cmd(&agent("aider", None), "fix it", "sess-1").unwrap()` (unchanged).
- `worker_cmd_quotes_single_quotes_in_prompt`: `reg().worker_cmd(&agent("codex", None), "don't break", "sess-1").unwrap()` (unchanged).
- `unknown_harness_runs_its_name_verbatim`: `reg().interactive_cmd(&agent("mytool", None), "sess-1")` and `reg().worker_cmd(&agent("mytool", None), "p", "sess-1")` (both unchanged — a synthesized spec has empty `interactive_args`/`None` `worker_args`).
- `freebuff_ships_disabled_without_worker_args`: `r.interactive_cmd(&agent("freebuff", Some("fb-large")), "sess-1")` (unchanged).
- `config_entry_replaces_builtin_spec`: `r.interactive_cmd(&agent("codex", Some("o3")), "sess-1")` (unchanged).
- `config_entry_extends_registry_with_new_harness`: `r.worker_cmd(&agent("freebuff2", None), "go", "sess-1").unwrap()` (unchanged).
- `agent_model_overrides_spec_model`: both `r.interactive_cmd(&agent("codex", Some("chosen")), "sess-1")` and `r.interactive_cmd(&agent("codex", None), "sess-1")` (both unchanged).

- [ ] **Step 2: Confirm compile failure**

Run: `cargo test -p ninox-core --lib 2>&1 | grep "this function takes"`
Expected: errors on `interactive_cmd`/`worker_cmd` call sites for taking too many arguments (2/3 expected, `N` supplied is backwards right now — the calls in Step 1 already pass 2-3 args against the *old* 1-2-arg signatures).

- [ ] **Step 3: Update `expand_args` to substitute `{session_id}`**

In `crates/ninox-core/src/harness.rs`:

```rust
fn expand_args(args: &[String], model: Option<&str>, prompt: Option<&str>, claude_session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let flag_for_dropped_model = model.is_none()
            && a.starts_with('-')
            && !a.contains('{')
            && args.get(i + 1).is_some_and(|n| n.contains("{model}"));
        if flag_for_dropped_model {
            i += 2;
            continue;
        }
        if a.contains("{model}") && model.is_none() {
            i += 1;
            continue;
        }
        let had_placeholder = a.contains("{model}") || a.contains("{prompt}") || a.contains("{session_id}");
        let mut s = a.replace("{model}", model.unwrap_or(""));
        if let Some(p) = prompt {
            s = s.replace("{prompt}", p);
        }
        s = s.replace("{session_id}", claude_session_id);
        out.push(if had_placeholder { shell_quote(&s) } else { s });
        i += 1;
    }
    out
}
```

(Only the function signature and the two new/changed lines — the `had_placeholder` check and the added `s = s.replace("{session_id}", ...)` — differ from today's version.)

- [ ] **Step 4: Update `interactive_cmd`/`worker_cmd` signatures**

```rust
    /// Interactive launch command (orchestrator / standalone sessions).
    /// `claude_session_id` is the UUID ninox generated for this spawn (see
    /// `new_claude_session_id`) — always required so every claude-code
    /// session is resumable from birth.
    pub fn interactive_cmd(&self, agent: &AgentConfig, claude_session_id: &str) -> String {
        let spec   = self.spec(&agent.harness);
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        join_cmd(binary, expand_args(&spec.interactive_args, model, None, claude_session_id))
    }

    /// Worker launch command, or `None` when the spec has no `worker_args`
    /// (worker mode unverified for this harness).
    pub fn worker_cmd(&self, agent: &AgentConfig, prompt: &str, claude_session_id: &str) -> Option<String> {
        let spec   = self.spec(&agent.harness);
        let wargs  = spec.worker_args.as_ref()?;
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        Some(join_cmd(binary, expand_args(wargs, model, Some(prompt), claude_session_id)))
    }
```

- [ ] **Step 5: Add `--session-id {session_id}` to the claude-code builtin spec**

In `builtin_specs()`:

```rust
    m.insert("claude-code".to_string(), HarnessSpec {
        enabled:          true,
        binary:           Some("claude".into()),
        interactive_args: vec!["--session-id".into(), "{session_id}".into(), "--model".into(), "{model}".into()],
        worker_args:      Some(vec![
            "--dangerously-skip-permissions".into(),
            "--session-id".into(), "{session_id}".into(),
            "--model".into(), "{model}".into(),
            "--".into(), "{prompt}".into(),
        ]),
        known_models:     vec![
            "claude-fable-5".into(),
            "claude-opus-4-8".into(),
            "claude-sonnet-5".into(),
            "claude-haiku-4-5".into(),
        ],
        ..HarnessSpec::default()
    });
```

(Only `interactive_args` and `worker_args` change; `codex`/`opencode`/`aider`/`freebuff` are untouched — they never get `{session_id}` in their arg templates, so `expand_args` is a no-op substitution for them.)

- [ ] **Step 6: Run the harness test suite**

Run: `cargo test -p ninox-core --lib harness:: -- --nocapture`
Expected: all pass, including the two rewritten and eight argument-count-updated tests from Step 1.

- [ ] **Step 7: Fix downstream compile errors (expected — deferred to Tasks 5–8)**

Run: `cargo build --workspace 2>&1 | grep "this function takes"`
Expected: remaining errors in `crates/ninox-app/src/{app.rs,main.rs}` — these are the real call sites, intentionally left broken here since fixing them requires the UUID-generation helper from Task 4. Confirm the errors are ONLY in `ninox-app` (not `ninox-core`) before moving on — `cargo test -p ninox-core --lib` (Step 6) already proved `ninox-core` itself is green.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(core): thread claude_session_id through interactive_cmd/worker_cmd"
```

(`ninox-app`/`ninox-server` remain non-compiling until Task 5 — this is a deliberately mid-flight commit inside one feature branch, not a shippable state. If your workflow prefers only-green commits, squash Tasks 3–7 before opening the PR; the task boundaries here are for review granularity, not deployability.)

---

### Task 4: Harness registry — `resume_args` and `resume_cmd`

**Files:**
- Modify: `crates/ninox-core/src/harness.rs` (`HarnessSpec`, `builtin_specs`, new `resume_cmd`, new `new_claude_session_id`)

**Interfaces:**
- Consumes: `expand_args` (Task 3), `HarnessSpec` (Task 3's shape plus this task's new field).
- Produces: `HarnessSpec.resume_args: Vec<String>` (empty = harness can't resume); `HarnessRegistry::resume_cmd(&self, agent: &AgentConfig, claude_session_id: &str) -> Option<String>`; `pub fn new_claude_session_id() -> String` — Tasks 5–8 call this at every spawn/resume call site.

- [ ] **Step 1: Write the failing tests**

Add to `crates/ninox-core/src/harness.rs`'s `mod tests`:

```rust
    #[test]
    fn resume_cmd_claude_code_with_model() {
        assert_eq!(
            reg().resume_cmd(&agent("claude-code", Some("claude-opus-4-5")), "sess-1").unwrap(),
            "claude --dangerously-skip-permissions --resume 'sess-1' --model 'claude-opus-4-5'"
        );
    }

    #[test]
    fn resume_cmd_claude_code_without_model() {
        assert_eq!(
            reg().resume_cmd(&agent("claude-code", None), "sess-1").unwrap(),
            "claude --dangerously-skip-permissions --resume 'sess-1'"
        );
    }

    #[test]
    fn resume_cmd_none_for_harness_without_resume_args() {
        // codex's builtin spec has no resume_args — resuming isn't supported.
        assert!(reg().resume_cmd(&agent("codex", Some("gpt-4o")), "sess-1").is_none());
    }

    #[test]
    fn new_claude_session_id_returns_distinct_uuids() {
        let a = new_claude_session_id();
        let b = new_claude_session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36); // UUID v4 string form, e.g. 8-4-4-4-12 hex groups
    }
```

- [ ] **Step 2: Confirm compile/test failure**

Run: `cargo test -p ninox-core --lib resume_cmd 2>&1 | tail -20`
Expected: `no method named 'resume_cmd' found` and `cannot find function 'new_claude_session_id'`.

- [ ] **Step 3: Add `resume_args` to `HarnessSpec` and the claude-code builtin**

In `crates/ninox-core/src/harness.rs`'s `HarnessSpec`, add after `worker_args`:

```rust
    /// Args for resuming a previously-interrupted session by its
    /// `claude_session_id`. Empty = this harness can't resume — a session
    /// under it never gets `SessionStatus::Interrupted` (see the reconciliation
    /// logic in `app.rs`) and never shows a Resume affordance.
    #[serde(default)]
    pub resume_args: Vec<String>,
```

In `builtin_specs()`, add to the `claude-code` entry (after `worker_args`, before `known_models`):

```rust
        resume_args: vec![
            "--dangerously-skip-permissions".into(),
            "--resume".into(), "{session_id}".into(),
            "--model".into(), "{model}".into(),
        ],
```

`HarnessSpec` already derives `Default`, and every other builtin spec (`codex`, `opencode`, `aider`, `freebuff`) is built with `..HarnessSpec::default()` — so they automatically get `resume_args: vec![]` with no changes needed.

- [ ] **Step 4: Add `resume_cmd` and `new_claude_session_id`**

In `HarnessRegistry`'s `impl` block, after `worker_cmd`:

```rust
    /// Resume command for a previously-interrupted session, or `None` when
    /// the harness has no `resume_args` (resume unsupported). Always
    /// rebuilds from `interactive`-style args (never `worker_args`) — a
    /// resumed session already has its original prompt in the transcript;
    /// replaying it via `worker_args`' trailing `-- {prompt}` would
    /// re-inject a stale instruction instead of continuing.
    pub fn resume_cmd(&self, agent: &AgentConfig, claude_session_id: &str) -> Option<String> {
        let spec = self.spec(&agent.harness);
        if spec.resume_args.is_empty() {
            return None;
        }
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        Some(join_cmd(binary, expand_args(&spec.resume_args, model, None, claude_session_id)))
    }
```

At the bottom of the file, before `#[cfg(test)]`:

```rust
/// A fresh UUID for `--session-id`/`--resume`. Generated by ninox at every
/// spawn (and at every Re-file, which discards the old conversation and
/// starts a new one) — never by the harness itself.
pub fn new_claude_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
```

- [ ] **Step 5: Add `uuid` as a direct dependency of `ninox-core`**

`uuid` is already present transitively (v1.23.4 in `Cargo.lock`, pulled in by another dependency) — this only promotes it to a direct, version-pinned dependency; it does not add a new third-party crate to the build.

Add to `crates/ninox-core/Cargo.toml`'s `[dependencies]`:

```toml
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p ninox-core --lib -- --nocapture`
Expected: all pass, including the four new tests.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(core): add resume_cmd and new_claude_session_id"
```

---

### Task 5: Wire UUID generation into `spawn_util.rs`

**Files:**
- Modify: `crates/ninox-app/src/spawn_util.rs` (`InteractiveSpawnParams`, `spawn_interactive_session`)
- Test: `crates/ninox-app/src/spawn_util.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `Session.claude_session_id` (Task 1).
- Produces: `InteractiveSpawnParams.claude_session_id: String` and `InteractiveSpawnParams.failure_status: SessionStatus` — Tasks 6–8 (Standalone, Orchestrator, Re-file) generate a UUID via `ninox_core::harness::new_claude_session_id()` and set `failure_status: SessionStatus::Terminated`; Task 10 (Resume) reuses the session's existing id and sets `failure_status: SessionStatus::Interrupted`.

**Note on `failure_status`:** the existing `spawn_interactive_session` unconditionally sets a session to `Terminated` when `tmux::create_session` fails (see its current body — the `if let Err(e) = tmux::create_session(...)` branch). That's correct for a fresh spawn or a Re-file (nothing to preserve), but wrong for Resume: per the spec's Error Handling section, a failed Resume must leave the session `Interrupted` (retryable), not `Terminated` (which hides the Resume button forever). This task makes the failure status a caller-supplied parameter instead of a hardcoded constant, so Task 10 can supply the right one.

- [ ] **Step 1: Write the failing tests**

Add to `crates/ninox-app/src/spawn_util.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn spawn_persists_claude_session_id() {
        use ninox_core::{config::AgentConfig, store::Store, SessionStatus};
        use tempfile::tempdir;

        let store = std::sync::Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = ninox_core::events::Engine::new(store.clone());
        let ws = tempdir().unwrap().keep().to_string_lossy().to_string();

        let attach = spawn_interactive_session(
            engine,
            InteractiveSpawnParams {
                session_id:        "spawn-uuid-test".into(),
                name:              "spawn-uuid-test".into(),
                workspace:         ws,
                repo:              String::new(),
                orchestrator_id:   None,
                agent:             AgentConfig::default(),
                base_cmd:          "sleep 30".into(),
                catalogue_path:    String::new(),
                extra_env:         Vec::new(),
                started_at:        0,
                claude_session_id: "fixed-uuid-for-test".into(),
                failure_status:    SessionStatus::Terminated,
            },
        )
        .await;
        assert!(attach.is_some(), "tmux create must succeed");

        let session = store.get_session("spawn-uuid-test").unwrap().unwrap();
        assert_eq!(session.claude_session_id.as_deref(), Some("fixed-uuid-for-test"));

        ninox_core::tmux::kill_session("spawn-uuid-test").await.ok();
    }

    #[tokio::test]
    async fn spawn_failure_uses_the_caller_supplied_failure_status() {
        use ninox_core::{config::AgentConfig, store::Store, types::Session, SessionStatus};
        use tempfile::tempdir;

        let store = std::sync::Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = ninox_core::events::Engine::new(store.clone());
        // Pre-insert the row exactly like the app's optimistic-insert-then-spawn
        // flow does, so the failure branch's `get_session`+update has a row to find.
        store.upsert_session(&Session {
            id: "spawn-fail-test".into(), orchestrator_id: None, name: "n".into(),
            repo: String::new(), status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None, model: None, context_tokens: None,
            catalogue_path: None, claude_session_id: Some("fixed-uuid".into()),
        }).unwrap();

        let attach = spawn_interactive_session(
            engine,
            InteractiveSpawnParams {
                session_id:        "spawn-fail-test".into(),
                name:              "n".into(),
                // A workspace that cannot exist makes tmux's `-c <workspace>` fail deterministically.
                workspace:         "/definitely/does/not/exist/ever".into(),
                repo:              String::new(),
                orchestrator_id:   None,
                agent:             AgentConfig::default(),
                base_cmd:          "sleep 30".into(),
                catalogue_path:    String::new(),
                extra_env:         Vec::new(),
                started_at:        0,
                claude_session_id: "fixed-uuid".into(),
                failure_status:    SessionStatus::Interrupted,
            },
        )
        .await;
        assert!(attach.is_none(), "tmux create must fail for a nonexistent workspace");

        let session = store.get_session("spawn-fail-test").unwrap().unwrap();
        assert!(
            matches!(session.status, SessionStatus::Interrupted),
            "failure_status must be honored, not hardcoded to Terminated",
        );
    }
```

- [ ] **Step 2: Confirm compile failure**

Run: `cargo test -p ninox-app --lib spawn_persists_claude_session_id 2>&1 | tail -20`
Expected: `struct InteractiveSpawnParams has no field named claude_session_id` (and, once that's fixed, no field `failure_status`).

- [ ] **Step 3: Add the fields and thread them through**

In `InteractiveSpawnParams`, add after `started_at`:

```rust
    pub started_at:      i64,
    /// UUID for `--session-id`/`--resume` (see `ninox_core::harness::new_claude_session_id`).
    /// Generated by the caller — this module never generates one itself,
    /// so `Re-file` (Task 8) can mint a fresh id while Resume (Task 10)
    /// reuses the session's existing one.
    pub claude_session_id: String,
    /// Status to set if `tmux::create_session` fails. `Terminated` for a
    /// fresh spawn or Re-file (nothing worth preserving); `Interrupted`
    /// for Resume (keep it retryable — see the design spec's Error
    /// Handling section).
    pub failure_status:  ninox_core::SessionStatus,
}
```

In `spawn_interactive_session`, update the existing failure branch to use `p.failure_status` instead of the hardcoded `SessionStatus::Terminated`:

```rust
    if let Err(e) = tmux::create_session(&sid, &p.workspace, &launch_cmd, &env).await {
        tracing::error!("tmux create failed for {sid}: {e}");
        if let Ok(Some(mut s)) = engine.store.get_session(&sid) {
            s.status = p.failure_status;
            let _ = engine.store.upsert_session(&s);
            engine.emit(Event::SessionUpdated(s));
        }
        return None;
    }
```

(Read the current body first — captured in full during planning — to confirm only the `s.status = ...` line changes; everything else in this branch is untouched.)

And the `Session` literal built after the pid lookup — replace the placeholder from Task 1 Step 7:

```rust
        catalogue_path:  (!p.catalogue_path.is_empty()).then(|| p.catalogue_path.clone()),
        claude_session_id: Some(p.claude_session_id),
    };
```

- [ ] **Step 4: Fix the two `#[ignore]`d probe tests' literals**

`spawn_probe` and `usage_ingestion_probe` in `spawn_util.rs`'s `persistence_probe` module both construct `InteractiveSpawnParams { ... started_at: 0, }` — add both new fields after `started_at: 0,` in both (these are `#[ignore]`d and won't run in CI, but must still compile):

```rust
                started_at:      0,
                claude_session_id: "probe-fixed-id".into(),
                failure_status:  ninox_core::SessionStatus::Terminated,
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ninox-app --lib spawn_persists_claude_session_id -- --nocapture`
Run: `cargo test -p ninox-app --lib spawn_failure_uses_the_caller_supplied_failure_status -- --nocapture`
Expected: both PASS (requires `tmux` installed locally — matches every other `tmux_available()`-gated test already in this suite; these have no such gate since `spawn_interactive_session` always calls tmux, matching the existing ungated tests in this same probe style).

Run: `cargo build --workspace 2>&1 | grep "this function takes\|missing field"`
Expected: remaining errors only in `crates/ninox-app/src/app.rs` (Standalone/Orchestrator/Re-file call sites — Tasks 6–8) and `main.rs` (Task 7's worker path uses `worker_cmd` directly, not this struct, but still needs its own fix).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(app): thread claude_session_id through spawn_interactive_session"
```

---

### Task 6: Wire UUID generation into `app.rs`'s Standalone and Orchestrator spawn paths

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (~line 900-906 shared agent/`base_cmd` setup, ~line 984 Standalone `Session` literal, ~line 1039-1050 Standalone `InteractiveSpawnParams`, ~line 1118 Orchestrator `Session` literal, ~line 1162-1179 Orchestrator `InteractiveSpawnParams`)

**Interfaces:**
- Consumes: `ninox_core::harness::new_claude_session_id()` (Task 4), `HarnessRegistry::interactive_cmd(&self, agent, claude_session_id: &str)` (Task 3), `InteractiveSpawnParams.claude_session_id` (Task 5).
- Produces: nothing new — this task is pure wiring.

- [ ] **Step 1: Write the failing test**

Add to `crates/ninox-app/src/app.rs`'s `mod tests`, right after the existing `spawn_form_confirm_inserts_orchestrator_and_navigates` test (it already drives `Message::SpawnSession` → `Message::SpawnFormName` → `Message::SpawnFormConfirm` to spawn an orchestrator named `"my-feature"`, whose id is `slugify("my-feature")` = `"my-feature"` — this new test is its near-twin, just asserting on `claude_session_id` instead of on the orchestrator list):

```rust
    #[test]
    fn orchestrator_spawn_persists_a_claude_session_id() {
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("my-feature".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        let orch_id = &m.orchestrators[0].id;
        let session = m.sessions.get(orch_id).expect("session recorded optimistically");
        assert!(
            session.claude_session_id.is_some(),
            "a fresh UUID must be recorded at spawn time",
        );
    }
```

- [ ] **Step 2: Confirm it fails**

Run: `cargo test -p ninox-app --lib orchestrator_spawn_persists_a_claude_session_id 2>&1 | tail -30`
Expected: FAIL — `session.claude_session_id` is `None` (still wired to the Task 1 Step 5 placeholder).

- [ ] **Step 3: Generate the UUID once, before the `SpawnKind` branch**

In `crates/ninox-app/src/app.rs`, right before the line `let base_cmd = registry.interactive_cmd(&agent);` (~line 906):

```rust
                let claude_session_id = ninox_core::harness::new_claude_session_id();
                let base_cmd = registry.interactive_cmd(&agent, &claude_session_id);
```

This one UUID is shared by whichever branch (`Standalone` or `Orchestrator`) runs next — it's independent of the human-chosen session name, so there's no reason to generate it twice.

- [ ] **Step 4: Use it in the Standalone branch**

At the Standalone `Session` literal (~line 984), replace the Task 1 placeholder:

```rust
                            catalogue_path:  Some(catalogue_path.clone()),
                            claude_session_id: Some(claude_session_id.clone()),
                        };
```

At the Standalone `InteractiveSpawnParams` (~line 1039-1050), add the field:

```rust
                                crate::spawn_util::InteractiveSpawnParams {
                                    session_id:      sid,
                                    name:            nm,
                                    workspace:       effective_ws,
                                    repo,
                                    orchestrator_id: None,
                                    agent,
                                    base_cmd,
                                    catalogue_path,
                                    extra_env:       Vec::new(),
                                    started_at:      ts_i64,
                                    claude_session_id,
                                    failure_status:  ninox_core::SessionStatus::Terminated,
                                },
```

(`claude_session_id` here is a plain `String`, and the closure already captures it by move along with `base_cmd`/`catalogue_path` — no `.clone()` needed since this is the last use. `failure_status` is `Terminated` here — a fresh spawn that fails to launch has nothing worth preserving.)

- [ ] **Step 5: Use it in the Orchestrator branch**

At the Orchestrator `Session` literal (~line 1118), replace the Task 1 placeholder:

```rust
                            catalogue_path:  Some(catalogue_path.clone()),
                            claude_session_id: Some(claude_session_id.clone()),
                        };
```

At the Orchestrator `InteractiveSpawnParams` (~line 1162-1179), add the field, and add `claude_session_id` to the `Task::future(async move { ... })`'s captured variables (find the block of `let sid = ...; let nm = ...; let orch_agent = agent;` right before the `Task::future` and add `let claude_session_id = claude_session_id;` alongside them — needed since it's moved into the async block):

```rust
                                crate::spawn_util::InteractiveSpawnParams {
                                    session_id:      sid,
                                    name:            nm,
                                    workspace:       ws,
                                    repo:            String::new(),
                                    orchestrator_id: None,
                                    agent:           orch_agent,
                                    base_cmd,
                                    catalogue_path,
                                    extra_env,
                                    started_at:      ts_i64,
                                    claude_session_id,
                                    failure_status:  ninox_core::SessionStatus::Terminated,
                                },
```

(Match this against the actual current field list at that call site — it may differ slightly in ordering/names from this sketch; add exactly two new lines, `claude_session_id,` and `failure_status: ninox_core::SessionStatus::Terminated,`, to whatever list is already there.)

- [ ] **Step 6: Run the tests**

Run: `cargo test -p ninox-app --lib orchestrator_spawn_persists_a_claude_session_id -- --nocapture`
Expected: PASS

Run: `cargo build --workspace 2>&1 | grep "this function takes\|missing field"`
Expected: remaining errors only in `main.rs` (Task 7) and the Re-file path in `app.rs` (Task 8).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(app): generate and persist claude_session_id on spawn"
```

---

### Task 7: Wire UUID generation into `main.rs`'s CLI worker path

**Files:**
- Modify: `crates/ninox-app/src/main.rs` (`run_spawn`)

**Interfaces:**
- Consumes: `ninox_core::harness::new_claude_session_id()` (Task 4), `HarnessRegistry::worker_cmd(&self, agent, prompt, claude_session_id: &str)` (Task 3).
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

`main.rs`'s `run_spawn` is a CLI entrypoint, not previously unit-tested in isolation per the code read during planning (it talks to a real `Store` and calls `tmux::create_session` directly). Rather than add new test scaffolding for a function that has none today, verify this task with the store round-trip instead: add to `crates/ninox-core/src/store.rs` is unnecessary (Task 1 already proved the column round-trips). Skip straight to a manual verification step:

- [ ] **Step 2: Make the change**

In `crates/ninox-app/src/main.rs`, right before `let session = Session { ... };` (~line 205):

```rust
    let claude_session_id = ninox_core::harness::new_claude_session_id();

    let session = Session {
```

Add the field to that literal, replacing the Task 1 placeholder:

```rust
        catalogue_path:  std::env::var("NINOX_BRAIN").ok().filter(|s| !s.is_empty()),
        claude_session_id: Some(claude_session_id.clone()),
    };
```

Update the `worker_cmd` call (~line 233-235):

```rust
    let cmd_base = registry
        .worker_cmd(&agent, &effective_prompt, &claude_session_id)
        .expect("worker-capability checked before any side effect above");
```

- [ ] **Step 3: Verify by compiling and running the existing CLI-path tests**

Run: `cargo build --workspace 2>&1 | grep "this function takes\|missing field"`
Expected: remaining errors only in the Re-file path in `app.rs` (Task 8).

Run: `cargo test --workspace`
Expected: all pass (this surfaces any test in `main.rs` — check first with `grep -n "mod tests" crates/ninox-app/src/main.rs`; if none exist, this step's job is just confirming the build is clean).

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(app): generate claude_session_id in the CLI worker spawn path"
```

---

### Task 8: `refile_plan`/`RefileSession` get a fresh `claude_session_id`

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (`RefilePlan`, `refile_plan`, `Message::RefileSession` handler, existing `refile_plan_*` tests)

**Interfaces:**
- Consumes: `ninox_core::harness::new_claude_session_id()` (Task 4), `HarnessRegistry::interactive_cmd` (Task 3), `InteractiveSpawnParams.claude_session_id` (Task 5).
- Produces: `refile_plan(session, is_orchestrator, config, claude_session_id: &str) -> Option<RefilePlan>` (new fourth parameter) — no other task depends on this signature.

- [ ] **Step 1: Update the failing tests for the new signature**

In `crates/ninox-app/src/app.rs`'s `mod tests`, update the two existing `refile_plan_*` tests to pass a fourth argument:

```rust
    #[test]
    fn refile_plan_resolves_agent_through_current_registry() {
        let mut cfg = ninox_core::config::AppConfig::default();
        cfg.harnesses.insert("claude-code".to_string(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary:  Some("claude-nightly".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..Default::default()
        });
        let session = refile_session("s1");
        let plan = refile_plan(&session, false, &cfg, "fresh-uuid").expect("plan");
        assert_eq!(plan.base_cmd, "claude-nightly --model 'claude-opus-4-8'");
        assert_eq!(plan.workspace, "/tmp/ws");
        assert_eq!(plan.catalogue_path, "/brains/b");
        assert!(plan.extra_env.is_empty());
        assert_eq!(plan.agent.harness, "claude-code");
        assert_eq!(plan.agent.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn refile_plan_orchestrator_gets_caller_env_and_no_workspace_means_no_plan() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("o1");
        session.catalogue_path = None;
        let plan = refile_plan(&session, true, &cfg, "fresh-uuid").expect("plan");
        assert!(plan.extra_env.iter().any(|(k, v)| k == "NINOX_ORCHESTRATOR_ID" && v == "o1"));
        assert!(plan.extra_env.iter().any(|(k, _)| k == "NINOX_CALLER_TYPE"));
        assert!(!plan.extra_env.iter().any(|(k, _)| k == "ATHENE_CALLER_TYPE" || k == "AO_CALLER_TYPE"));
        assert!(!plan.catalogue_path.is_empty());

        session.workspace_path = None;
        assert!(refile_plan(&session, true, &cfg, "fresh-uuid").is_none());
    }
```

Add a new test proving each Re-file gets a genuinely fresh id, not the session's old one:

```rust
    #[test]
    fn refile_plan_uses_the_freshly_generated_id_not_the_stale_one() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stale-id-from-before".into());
        let plan = refile_plan(&session, false, &cfg, "brand-new-id").expect("plan");
        assert!(plan.base_cmd.contains("brand-new-id"));
        assert!(!plan.base_cmd.contains("stale-id-from-before"));
    }
```

- [ ] **Step 2: Confirm compile/test failure**

Run: `cargo test -p ninox-app --lib refile_plan 2>&1 | tail -30`
Expected: `this function takes 3 arguments but 4 arguments were supplied` (from the two updated tests) and a subsequent failure for the new test once that's fixed.

- [ ] **Step 3: Update `refile_plan`'s signature**

```rust
pub fn refile_plan(
    session: &Session,
    is_orchestrator: bool,
    config: &AppConfig,
    claude_session_id: &str,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let agent = ninox_core::config::AgentConfig {
        harness: session.agent_type.clone(),
        model:   session.model.clone(),
    };
    let base_cmd = config.registry().interactive_cmd(&agent, claude_session_id);
    let catalogue_path = session.catalogue_path.clone()
        .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, catalogue_path, extra_env })
}
```

(Only the new parameter and the `interactive_cmd` call change — confirm the rest against the function's actual current body, read in full during planning, before editing.)

- [ ] **Step 4: Update the `Message::RefileSession` handler**

In the `Message::RefileSession(id)` arm, generate the fresh UUID before calling `refile_plan`, and pass it through to `InteractiveSpawnParams`:

```rust
            Message::RefileSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let claude_session_id = ninox_core::harness::new_claude_session_id();
                let Some(plan) = refile_plan(&session, is_orch, &state.config, &claude_session_id) else {
                    tracing::warn!("refile {id}: no workspace recorded, cannot respawn");
                    return Task::none();
                };
                state.clients.remove(&id);
                state.terminals.remove(&id);

                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let engine  = state.engine.clone();
                let name    = session.name.clone();
                let repo    = session.repo.clone();
                let orch_id = session.orchestrator_id.clone();
                Task::future(async move {
                    let _ = ninox_core::tmux::kill_session(&id).await;
                    let attach = crate::spawn_util::spawn_interactive_session(
                        engine,
                        crate::spawn_util::InteractiveSpawnParams {
                            session_id:      id.clone(),
                            name,
                            workspace:       plan.workspace,
                            repo,
                            orchestrator_id: orch_id,
                            agent:           plan.agent,
                            base_cmd:        plan.base_cmd,
                            catalogue_path:  plan.catalogue_path,
                            extra_env:       plan.extra_env,
                            started_at:      ts,
                            claude_session_id,
                            failure_status:  ninox_core::SessionStatus::Terminated,
                        },
                    )
                    .await;
                    match attach {
                        Some(argv) => Message::ClientAttach { session_id: id, argv },
                        None       => Message::Noop,
                    }
                })
            }
```

(`failure_status: Terminated` — Re-file explicitly discards the prior conversation and starts fresh, so a failed relaunch has nothing worth preserving either, same as a first-time spawn.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ninox-app --lib refile_plan -- --nocapture`
Expected: all pass, including the new staleness test.

Run: `cargo build --workspace`
Expected: clean — this was the last remaining call site from Tasks 3–5's deferred breakage.

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(app): Re-file generates a fresh claude_session_id"
```

---

### Task 9: Startup reconciliation — `Interrupted` vs `Terminated`, and the five terminal-state guards

**This is the correctness-critical task in this plan — see the Global Constraints note at the top before starting.**

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (`App::new`'s startup reconciliation task, ~line 430-458)
- Modify: `crates/ninox-core/src/lifecycle/poller.rs` (`poll_pids`, `sync_sessions_metadata`, `poll_usage`, `poll_github`)
- Test: `crates/ninox-app/src/app.rs` (new startup-reconciliation tests), `crates/ninox-core/src/lifecycle/poller.rs` (new regression tests)

**Interfaces:**
- Consumes: `SessionStatus::Interrupted` (Task 2), `Session.claude_session_id` (Task 1), `HarnessRegistry::resume_cmd` returning `None` for non-resumable harnesses (Task 4).
- Produces: the reconciliation rule itself — Task 10 (resume action) is only ever offered to a session in this state.

- [ ] **Step 1: Write the failing reconciliation tests**

`App::new`'s startup task currently isn't unit-tested directly as a function (it's inline in `App::new`'s returned `Task::future`) — extract its decision logic into a small, directly-testable pure function first, since that's what makes this testable without spinning up a real tmux server. Add to `crates/ninox-app/src/app.rs`, near `refile_plan` (same "pure decision logic, factored out of the async/IO shell" pattern):

```rust
/// Decide what a session's status becomes when its tmux pane is found
/// gone at startup. `has_resume_args` is the harness's capability (from
/// `HarnessRegistry::resume_cmd(...).is_some()` against a placeholder id —
/// callers don't have a real command to build yet, just the capability
/// check), not whether resume has ever been attempted.
fn reconciled_status_for_dead_session(
    claude_session_id: &Option<String>,
    has_resume_args:   bool,
) -> SessionStatus {
    if claude_session_id.is_some() && has_resume_args {
        SessionStatus::Interrupted
    } else {
        SessionStatus::Terminated
    }
}
```

Add tests directly below it:

```rust
#[cfg(test)]
mod reconciliation_tests {
    use super::*;

    #[test]
    fn session_with_id_and_resumable_harness_becomes_interrupted() {
        assert_eq!(
            reconciled_status_for_dead_session(&Some("uuid-1".into()), true),
            SessionStatus::Interrupted,
        );
    }

    #[test]
    fn legacy_session_without_id_becomes_terminated() {
        assert_eq!(
            reconciled_status_for_dead_session(&None, true),
            SessionStatus::Terminated,
        );
    }

    #[test]
    fn session_under_non_resumable_harness_becomes_terminated_even_with_an_id() {
        assert_eq!(
            reconciled_status_for_dead_session(&Some("uuid-1".into()), false),
            SessionStatus::Terminated,
        );
    }
}
```

- [ ] **Step 2: Confirm it fails to compile**

Run: `cargo test -p ninox-app --lib reconciliation_tests 2>&1 | tail -20`
Expected: `cannot find function 'reconciled_status_for_dead_session'`.

- [ ] **Step 3: The function from Step 1 IS the implementation — just add it (not inside `#[cfg(test)]`)**

Add the `reconciled_status_for_dead_session` function itself (the non-test part of Step 1's code block above) to `app.rs` as a real, non-test item — e.g. directly above `refile_plan`.

- [ ] **Step 4: Run the new tests**

Run: `cargo test -p ninox-app --lib reconciliation_tests -- --nocapture`
Expected: all 3 PASS.

- [ ] **Step 5: Wire it into `App::new`'s startup task**

Replace the body of the startup `Task::future` in `App::new` (currently: tmux session gone → unconditionally `Terminated`):

```rust
        let task = Task::future(async move {
            use ninox_core::{tmux, Event as CoreEvent, SessionStatus};

            let sessions = match engine.store.list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("restore: list_sessions: {e}");
                    return Message::Noop;
                }
            };
            let registry = AppConfig::load().unwrap_or_default().registry();

            for session in sessions {
                if matches!(
                    session.status,
                    SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted
                ) {
                    continue;
                }

                if !tmux::has_session(&session.id).await {
                    let agent = ninox_core::config::AgentConfig {
                        harness: session.agent_type.clone(),
                        model:   session.model.clone(),
                    };
                    let has_resume_args = registry.resume_cmd(&agent, "placeholder").is_some();
                    let mut dead = session.clone();
                    dead.status = reconciled_status_for_dead_session(
                        &session.claude_session_id, has_resume_args,
                    );
                    let _ = engine.store.upsert_session(&dead);
                    engine.emit(CoreEvent::SessionUpdated(dead));
                }
            }

            Message::Noop
        });
```

Read the actual current body first (it was captured in full during planning — reproduced above) to confirm the `engine`/`sessions` capture matches. Note `App::new`'s synchronous half already loads config into a local `config` binding just above this task's `Task::future(async move { ... })` — but that binding is moved into `app.config` (the returned `Self { config, ... }` literal) before the closure runs, so it is *not* available inside the async block; calling `AppConfig::load()` again here is the only option, not a redundant reload to avoid.

- [ ] **Step 6: Add `Interrupted` to the four `poller.rs` guards**

In `crates/ninox-core/src/lifecycle/poller.rs`, there are four occurrences of `matches!(session.status, SessionStatus::Done | SessionStatus::Terminated)` — one each in `poll_pids`, `sync_sessions_metadata`, `poll_usage`, `poll_github`. Change all four to:

```rust
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
```

(Identical replacement text at all four sites — grep to confirm you got all four: `grep -n "Done | SessionStatus::Terminated" crates/ninox-core/src/lifecycle/poller.rs` should return zero matches after this step, since every remaining occurrence must include `Interrupted` too.)

- [ ] **Step 7: Write the poller regression test proving `Interrupted` survives**

Add to `crates/ninox-core/src/lifecycle/poller.rs`'s `mod tests`, using the existing `test_session` helper:

```rust
    #[tokio::test]
    async fn poll_pids_leaves_interrupted_sessions_alone() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("interrupted-1", "/ws");
        s.status = SessionStatus::Interrupted;
        s.pid = Some(999_999); // a pid that is almost certainly dead
        store.upsert_session(&s).unwrap();
        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.poll_pids().await;

        let after = store.get_session("interrupted-1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::Interrupted),
            "poll_pids must not re-terminate an Interrupted session just because its stale pid is dead",
        );
    }
```

(`test_session` already exists in this file's test module from the earlier `poll_usage_*` tests — reuse it rather than redefining. If `SessionStatus` needs importing in this test block, it's already imported at the top of `mod tests` per the existing `derive_status_*` tests.)

- [ ] **Step 8: Run the tests**

Run: `cargo test -p ninox-core --lib poll_pids_leaves_interrupted_sessions_alone -- --nocapture`
Expected: PASS

Run: `cargo test --workspace`
Expected: all pass — this includes every pre-existing `poller.rs` and `app.rs` test, proving the guard changes didn't alter behavior for `Done`/`Terminated`/anything else.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "fix(core): distinguish Interrupted from Terminated on tmux-server death"
```

---

### Task 10: `resume_plan` and `Message::ResumeSession`

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (new `Message::ResumeSession(SessionId)` variant, new `resume_plan` function mirroring `refile_plan`, new update-arm mirroring `RefileSession`)
- Test: `crates/ninox-app/src/app.rs`

**Interfaces:**
- Consumes: `HarnessRegistry::resume_cmd` (Task 4), `SessionStatus::Interrupted` (Task 2/9), `InteractiveSpawnParams` (Task 5).
- Produces: `Message::ResumeSession(SessionId)`, `resume_plan(session: &Session, is_orchestrator: bool, config: &AppConfig) -> Option<RefilePlan>` — Task 11 (UI button) dispatches this message; Task 12 (bulk resume) fans it out over every `Interrupted` session id.

- [ ] **Step 1: Write the failing tests**

Add near the `refile_plan_*` tests in `crates/ninox-app/src/app.rs`'s `mod tests`:

```rust
    #[test]
    fn resume_plan_builds_a_resume_command_from_the_stored_id() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stored-uuid".into());
        let plan = resume_plan(&session, false, &cfg).expect("plan");
        assert!(plan.base_cmd.contains("--resume"));
        assert!(plan.base_cmd.contains("stored-uuid"));
        assert_eq!(plan.workspace, "/tmp/ws");
    }

    #[test]
    fn resume_plan_none_without_a_stored_claude_session_id() {
        let cfg = ninox_core::config::AppConfig::default();
        let session = refile_session("s1"); // claude_session_id: None by default in this fixture
        assert!(resume_plan(&session, false, &cfg).is_none());
    }

    #[test]
    fn resume_message_relaunches_and_attaches() {
        let e = test_engine();
        let mut m = base(e);
        let mut s = refile_session("s1");
        s.status = SessionStatus::Interrupted;
        s.claude_session_id = Some("stored-uuid".into());
        m.sessions.insert("s1".into(), s.clone());
        m.engine.store.upsert_session(&s).unwrap();
        let (_m, task) = m.update(Message::ResumeSession("s1".into()));
        // A real assertion needs the async Task to run against a real tmux
        // server, matching the pattern `refile_message_drops_client_state_and_keeps_session`
        // already uses nearby — read that test's exact assertion shape (does
        // it execute the Task, or only check synchronous state like
        // `clients`/`terminals` being cleared?) and mirror it precisely
        // rather than guessing; if it only checks synchronous state, this
        // test should too, since `Task` execution isn't observable from a
        // plain `update()` call.
        let _ = task;
    }
```

Before finalizing `resume_message_relaunches_and_attaches`, open `refile_message_drops_client_state_and_keeps_session` (~line 2581 per planning) and copy its actual assertion style — it exists specifically to prove this pattern already works for Re-file, so this test should be its near-twin, not a reinvention.

- [ ] **Step 2: Confirm compile failure**

Run: `cargo test -p ninox-app --lib resume_plan 2>&1 | tail -30`
Expected: `cannot find function 'resume_plan'`, `no variant named 'ResumeSession'`.

- [ ] **Step 3: Add `resume_plan`**

Right after `refile_plan`:

```rust
/// Like `refile_plan`, but rebuilds via `resume_cmd` (continuing the
/// existing `claude_session_id`) instead of `interactive_cmd` (starting
/// fresh). `None` when there's no workspace to resume into OR no stored
/// `claude_session_id` OR the harness can't resume (`resume_cmd` returns
/// `None`) — any of which means there's nothing to relaunch into.
pub fn resume_plan(
    session: &Session,
    is_orchestrator: bool,
    config: &AppConfig,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let claude_session_id = session.claude_session_id.as_deref()?;
    let agent = ninox_core::config::AgentConfig {
        harness: session.agent_type.clone(),
        model:   session.model.clone(),
    };
    let base_cmd = config.registry().resume_cmd(&agent, claude_session_id)?;
    let catalogue_path = session.catalogue_path.clone()
        .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, catalogue_path, extra_env })
}
```

- [ ] **Step 4: Add the `Message` variant and update-arm**

Add `ResumeSession(SessionId),` to the `Message` enum, next to `RefileSession(SessionId),`.

Add the update-arm right after the `Message::RefileSession` arm — it mirrors that arm almost exactly, using `resume_plan` in place of `refile_plan`, generating a fresh `claude_session_id` only if `resume_plan` itself doesn't already carry one forward (it doesn't need to — resuming reuses the *existing* id, unlike Re-file):

```rust
            Message::ResumeSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let Some(plan) = resume_plan(&session, is_orch, &state.config) else {
                    tracing::warn!("resume {id}: no workspace/claude_session_id/resume-capable harness, cannot resume");
                    return Task::none();
                };
                let Some(claude_session_id) = session.claude_session_id.clone() else {
                    return Task::none(); // unreachable: resume_plan already required this
                };
                state.clients.remove(&id);
                state.terminals.remove(&id);

                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let engine  = state.engine.clone();
                let name    = session.name.clone();
                let repo    = session.repo.clone();
                let orch_id = session.orchestrator_id.clone();
                Task::future(async move {
                    let _ = ninox_core::tmux::kill_session(&id).await;
                    let attach = crate::spawn_util::spawn_interactive_session(
                        engine,
                        crate::spawn_util::InteractiveSpawnParams {
                            session_id:      id.clone(),
                            name,
                            workspace:       plan.workspace,
                            repo,
                            orchestrator_id: orch_id,
                            agent:           plan.agent,
                            base_cmd:        plan.base_cmd,
                            catalogue_path:  plan.catalogue_path,
                            extra_env:       plan.extra_env,
                            started_at:      ts,
                            claude_session_id,
                            failure_status:  SessionStatus::Interrupted,
                        },
                    )
                    .await;
                    match attach {
                        Some(argv) => Message::ClientAttach { session_id: id, argv },
                        None       => Message::Noop,
                    }
                })
            }
```

(`failure_status: Interrupted` — this is the whole point of Task 5's change: a Resume that fails to launch must stay `Interrupted`, not fall back to `Terminated`, so the user can retry instead of losing the row. Add a regression test for exactly this, mirroring Task 5's `spawn_failure_uses_the_caller_supplied_failure_status`:

```rust
    #[test]
    fn resume_message_keeps_status_interrupted_when_tmux_create_fails() {
        let e = test_engine();
        let mut m = base(e);
        let mut s = refile_session("s1");
        s.status = SessionStatus::Interrupted;
        s.claude_session_id = Some("stored-uuid".into());
        s.workspace_path = Some("/definitely/does/not/exist/ever".into());
        m.sessions.insert("s1".into(), s.clone());
        m.engine.store.upsert_session(&s).unwrap();
        let (_m, task) = m.update(Message::ResumeSession("s1".into()));
        // As in `resume_message_relaunches_and_attaches`, running `task` to
        // completion needs a real async executor — mirror whichever
        // execution helper `refile_message_drops_client_state_and_keeps_session`
        // (or its neighbors) already uses in this file to drive a `Task` to
        // completion in a test, then assert:
        // let session = m.engine.store.get_session("s1").unwrap().unwrap();
        // assert!(matches!(session.status, SessionStatus::Interrupted));
        let _ = task;
    }
```

Before finalizing this test, check whether any existing test in `app.rs` already drives a `Task::future` to completion synchronously (search for `#[tokio::test]` near `RefileSession`/`SpawnFormConfirm` tests) — if one does, copy its exact execution pattern instead of leaving the assertion commented out as above.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ninox-app --lib resume_plan -- --nocapture`
Run: `cargo test -p ninox-app --lib resume_message_relaunches_and_attaches -- --nocapture`
Run: `cargo test -p ninox-app --lib resume_message_keeps_status_interrupted_when_tmux_create_fails -- --nocapture`
Expected: all PASS.

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(app): add resume_plan and Message::ResumeSession"
```

---

### Task 11: UI — Resume button in `session_detail.rs`

**Files:**
- Modify: `crates/ninox-app/src/components/session_detail.rs` (header row, next to `refile_btn`)

**Interfaces:**
- Consumes: `Message::ResumeSession` (Task 10), `SessionStatus::Interrupted` (Task 2).
- Produces: nothing new — leaf UI.

- [ ] **Step 1: Write the failing test**

This repo's component tests (per `fleet_board.rs`'s `filtered_sessions` pattern seen during planning) tend to test pure helper functions rather than full widget trees. Since the Resume button's only real logic is "shown/enabled iff `session.status == Interrupted`," extract that as a tiny pure predicate right above where `refile_btn` is built, and test it directly:

```rust
    #[test]
    fn can_resume_only_when_interrupted() {
        use ninox_core::types::SessionStatus;
        assert!(can_resume(&SessionStatus::Interrupted));
        assert!(!can_resume(&SessionStatus::Working));
        assert!(!can_resume(&SessionStatus::Terminated));
        assert!(!can_resume(&SessionStatus::Done));
    }
```

(Add this to `session_detail.rs`'s test module — if none exists yet, check first with `grep -n "mod tests" crates/ninox-app/src/components/session_detail.rs`; if absent, add `#[cfg(test)] mod tests { use super::*; ... }` at the bottom of the file, matching the style of `fleet_board.rs`'s or `style.rs`'s test modules.)

- [ ] **Step 2: Confirm it fails**

Run: `cargo test -p ninox-app --lib can_resume_only_when_interrupted 2>&1 | tail -20`
Expected: `cannot find function 'can_resume'`.

- [ ] **Step 3: Add `can_resume` and the button**

Add the predicate near the top of the file (or right before its first use, matching this file's existing style — `refile_btn`'s `can_refile` is inlined at its use site as a `let`, so a small standalone `fn` is a deliberate, testable departure only because this task needs it unit-tested independent of the widget tree):

```rust
fn can_resume(status: &SessionStatus) -> bool {
    matches!(status, SessionStatus::Interrupted)
}
```

Add the button right after `refile_btn`'s block (before `kill_btn`):

```rust
    let resume_btn: Element<Message> = if can_resume(&session.status) {
        let sid = session_id.to_string();
        button(crate::style::micro_label("Resume", s.status_review).size(10.0))
            .on_press(Message::ResumeSession(sid))
            .padding([6, 16])
            .style(move |_theme, status| {
                let hovered = matches!(status, button::Status::Hovered);
                button::Style {
                    background: hovered.then_some(Background::Color(s.status_review)),
                    text_color: if hovered { s.card } else { s.status_review },
                    border: Border { color: s.status_review, width: 1.5, radius: 2.0.into() },
                    shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
                }
            })
            .into()
    } else {
        Space::new(0, 0).into()
    };
```

Add it to the header `row![...]`, right after `refile_btn` and its `Space::new(10, 0),`:

```rust
            refile_btn,
            Space::new(10, 0),
            resume_btn,
            Space::new(10, 0),
            kill_btn,
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ninox-app --lib can_resume_only_when_interrupted -- --nocapture`
Expected: PASS

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(ui): Resume button on interrupted sessions"
```

---

### Task 12: UI — Fleet board badge, column placement, and bulk "Resume all"

**Files:**
- Modify: `crates/ninox-app/src/components/fleet_board.rs` (`board_sessions`/`COLUMNS` merge for `Interrupted`, folio's shown-count sum, new bulk resume control)
- Modify: `crates/ninox-app/src/app.rs` (new `Message::ResumeAllSessions`)

**Interfaces:**
- Consumes: `SessionStatus::Interrupted` (Task 2), `Message::ResumeSession` (Task 10), `stamp_word`/`status_color` (Task 2).
- Produces: `Message::ResumeAllSessions` — fans out to one `Message::ResumeSession` per interrupted session; no later task depends on this.

- [ ] **Step 1: Write the failing test for the merge rule**

`fleet_board.rs`'s test module (`mod tests`, currently just `folio_title_follows_time_of_day`) has no App-building helper of its own, and the merge logic lives inline in `folio`'s rendering closure — not in a unit-testable function. Extract the merge rule into a small pure function first (same pattern as Task 11's `can_resume`), so this needs no `App`/store/tmux at all:

```rust
    #[test]
    fn column_absorbs_matches_done_terminated_and_working_interrupted() {
        use SessionStatus::*;
        assert!(column_absorbs(&Done, &Terminated));
        assert!(column_absorbs(&Working, &Interrupted));
        assert!(column_absorbs(&Working, &Working)); // exact match still counts
        assert!(!column_absorbs(&Done, &Interrupted));
        assert!(!column_absorbs(&Working, &Terminated));
        assert!(!column_absorbs(&PrOpen, &Terminated));
    }
```

- [ ] **Step 2: Confirm it fails to compile**

Run: `cargo test -p ninox-app --lib column_absorbs_matches_done_terminated_and_working_interrupted 2>&1 | tail -20`
Expected: `cannot find function 'column_absorbs'`.

- [ ] **Step 3: Add `column_absorbs`, and a `column_sessions` sibling to `board_sessions` that uses it**

Add near `board_sessions`:

```rust
/// Whether a session with `session_status` is shown under a column whose
/// header status is `col_status` — either an exact match, or one of the
/// two "closed-ish"/"stuck-ish" foldings the board applies: `Terminated`
/// sessions appear under `Done`, and `Interrupted` sessions appear under
/// `Working` (a session that was mid-task and got cut off belongs with
/// the sessions still working, not with the ones that finished).
fn column_absorbs(col_status: &SessionStatus, session_status: &SessionStatus) -> bool {
    use SessionStatus::*;
    session_status == col_status
        || (*col_status == Done && *session_status == Terminated)
        || (*col_status == Working && *session_status == Interrupted)
}

/// Like `board_sessions`, but matches via `column_absorbs` instead of exact
/// status equality — this is what a column actually renders. Duplicates
/// `board_sessions`' query/orchestrator/scope filtering by design rather
/// than calling it twice and merging (Terminated-under-Done was previously
/// done that way, via a base call plus a conditional `.extend()` — adding
/// a second folded status that way would mean a third additive call, and
/// still wouldn't help `folio`'s `shown` count reuse the same rule).
pub fn column_sessions<'a>(
    app: &'a App,
    col_status: &SessionStatus,
    scope: Option<&str>,
) -> Vec<&'a Session> {
    let q = app.fleet_filter.query.to_lowercase();
    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let mut sessions: Vec<&Session> = app.sessions.values().filter(|s| {
        column_absorbs(col_status, &s.status)
            && !orch_ids.contains(s.id.as_str())
            && scope.is_none_or(|oid| s.orchestrator_id.as_deref() == Some(oid))
            && (q.is_empty()
                || s.name.to_lowercase().contains(&q)
                || s.repo.to_lowercase().contains(&q))
    }).collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}
```

In the column-rendering loop (captured in full during planning: it currently calls `let mut col_sessions = board_sessions(app, &col.status, scope.map(|s| s.as_str()));` then conditionally `.extend(board_sessions(app, &SessionStatus::Terminated, ...))` when `col.status == Done`), replace both lines with one call:

```rust
                let col_sessions = column_sessions(app, &col.status, scope.map(|s| s.as_str()));
```

(The rest of that closure — `.iter().map(|s| session_card(app, s)).collect()` then `ledger_column(...)` — is unchanged; only the two lines that built `col_sessions` collapse into this one.)

Update the folio's `shown` count (captured in full during planning: `COLUMNS.iter().map(|c| board_sessions(app, &c.status, ...).len()).sum::<usize>() + board_sessions(app, &SessionStatus::Terminated, ...).len()`) to use `column_sessions` instead of `board_sessions`, which drops the separate `+ ...Terminated` term entirely — `column_sessions` already folds both `Terminated` (under `Done`) and `Interrupted` (under `Working`) into whichever of the six `COLUMNS` entries absorbs them:

```rust
    let shown = COLUMNS.iter()
        .map(|c| column_sessions(app, &c.status, scope.map(|x| x.as_str())).len())
        .sum::<usize>();
```

`board_sessions` itself is untouched by this step and keeps its existing callers (e.g. `attention_count` still needs exact-status matching, not the column-merge rule).

- [ ] **Step 4: Add the bulk "Resume all (N)" control and `Message::ResumeAllSessions`**

In `crates/ninox-app/src/app.rs`, add `ResumeAllSessions,` to the `Message` enum next to `ResumeSession`, and its update-arm right after `Message::ResumeSession`'s:

```rust
            Message::ResumeAllSessions => {
                let ids: Vec<SessionId> = state.sessions.values()
                    .filter(|s| matches!(s.status, SessionStatus::Interrupted))
                    .map(|s| s.id.clone())
                    .collect();
                Task::batch(ids.into_iter().map(|id| {
                    Task::done(Message::ResumeSession(id))
                }))
            }
```

In `fleet_board.rs`, add a small helper (near `attention_count`) and the button (in the `folio` header row, right after the date label — only rendered when there's something to resume):

```rust
pub fn interrupted_count(app: &App) -> usize {
    app.sessions.values().filter(|s| matches!(s.status, SessionStatus::Interrupted)).count()
}
```

```rust
                text(date_label.clone())
                    .size(10)
                    .font(crate::style::MONO)
                    .color(s.faint),
```
becomes (inserting the conditional button right after the date label text widget, inside the same `row![...]`):
```rust
                text(date_label.clone())
                    .size(10)
                    .font(crate::style::MONO)
                    .color(s.faint),
                {
                    let n = interrupted_count(app);
                    if n > 0 {
                        Space::new(14, 0)
                    } else {
                        Space::new(0, 0)
                    }
                },
                if interrupted_count(app) > 0 {
                    let n = interrupted_count(app);
                    button(crate::style::micro_label(&format!("Resume all ({n})"), s.status_review).size(9.5))
                        .on_press(Message::ResumeAllSessions)
                        .padding([4, 10])
                        .style(move |_theme, status| {
                            let hovered = matches!(status, button::Status::Hovered);
                            iced::widget::button::Style {
                                background: hovered.then_some(Background::Color(s.status_review)),
                                text_color: if hovered { s.card } else { s.status_review },
                                border: Border { color: s.status_review, width: 1.5, radius: 2.0.into() },
                                shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
                            }
                        })
                        .into()
                } else {
                    Element::<Message>::from(Space::new(0, 0))
                },
```

Read the actual current `row![...]` contents around the date label first (captured in full during planning, reproduced near the top of this file's `folio` function) — insert these two new elements into that exact `row!` macro invocation rather than assuming its surrounding punctuation matches this sketch verbatim.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ninox-app --lib column_absorbs_matches_done_terminated_and_working_interrupted -- --nocapture`
Expected: PASS

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(ui): Interrupted badge, Working-column placement, and bulk Resume all"
```

---

### Task 13: Full verification and PR readiness

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build and test**

Run: `cargo build --workspace 2>&1 | tail -30`
Expected: clean build, no warnings about the new code (fix any that appear before proceeding).

Run: `cargo test --workspace -- --nocapture 2>&1 | tail -60`
Expected: all tests pass, including every test added across Tasks 1–12.

- [ ] **Step 2: Manual smoke test against a real tmux server**

Run: `cargo run -p ninox-app` (or however this repo's `/run` skill launches it — check for a project-level `run` skill first per this session's tooling), spawn a standalone session, confirm in a separate terminal that `tmux -L ninox list-sessions` shows it, then kill the tmux server entirely (`tmux -L ninox kill-server`) to simulate a reboot, relaunch the app, and confirm:
- The session shows as "Interrupted" (not "Closed") in the Fleet board.
- The session_detail view shows a "Resume" button.
- Clicking Resume relaunches the pane and the status returns to "Working".

This step has no automated assertion — it's the one point in this plan validating the real `claude --resume` round-trip end-to-end, which no unit test can cover (it needs an actual `claude` CLI session with a real transcript).

- [ ] **Step 3: Review the diff against the spec**

Read `docs/superpowers/specs/2026-07-06-session-resume-design.md` once more against `git diff main...HEAD` — confirm every section (Data model, Harness registry, Spawn-time change, Startup reconciliation, UI, Error handling) has corresponding code, and that the five terminal-state guards (Global Constraints) are all present:

Run: `grep -rn "Done | SessionStatus::Terminated" crates/ninox-core/src/lifecycle/poller.rs crates/ninox-app/src/app.rs`
Expected: **zero** matches — every surviving occurrence must include `Interrupted` in the pattern (confirms Task 9 didn't miss one).

- [ ] **Step 4: Final commit (if Step 1–3 required fixes)**

```bash
git add -A
git commit -m "chore: fix warnings and gaps found in final verification"
```

(Skip this commit if Steps 1–3 needed no changes.)
