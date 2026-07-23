# Lifecycle Gate Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the `Event::SessionUpdated` write-race class of bug, add a structured `GateStatus` (CI/review/mergeable) that's surfaced via hover tooltip, and make the done/retention cleanup window visible as an always-on badge in the sidebar and fleet board.

**Architecture:** Three sequential layers in `ninox-core`: (1) a field-level merge on the frontend's event-apply path so a stale rebroadcast from one poller tick can never stomp a fresher field written by another, (2) a `GateStatus` struct computed from data `poll_github` already fetches and persisted alongside `SessionStatus`, (3) UI: `iced::widget::tooltip` wrapping each status indicator (sidebar, fleet board, PR list) plus an always-visible retention countdown badge (sidebar, fleet board) computed client-side from the existing `terminal_at` field.

**Tech Stack:** Rust, Iced 0.13 (native GUI), SQLite via `rusqlite`, Tokio.

## Global Constraints

- No new crate dependencies (per spec non-goals: no bitflags crate — `SessionFields` is a small hand-rolled bitset).
- `Store::upsert_session` stays a full-row upsert; only the frontend's event-apply path changes from replace to merge (per spec §2).
- `Session::merge_from`/`GateStatus` reflect current state only — no transition history table (per spec non-goals).
- "Done" stays defined solely by `pr_status.merged` — no change to `handle_merge_detection`/`derive_session_status`'s terminal-state logic (per spec non-goals).
- Retention/cleanup logic (`sweep_retired_sessions`, `terminate_session`, `remove_session`, `cleanup_session`) is unchanged — only its existing `terminal_at` field gets a new UI reader (per spec non-goals).
- One deliberate deviation from the spec's UI sketch, discovered during planning: the spec's "Wiring" paragraph sketches a manual `mouse_area`/`Message::GateHover`/`App.hovered_gate` hover-tracking mechanism, modeled on `brain_panel.rs`'s canvas-specific `hover_preview_slip`. That pattern exists only because the brain pinboard is a `canvas` widget with no built-in hover primitive. Sidebar/fleet-board/PR-list rows are ordinary widget trees, where Iced already provides `iced::widget::tooltip` (`iced_widget::tooltip::Tooltip`, confirmed present in the vendored `iced_widget 0.13.4` at `tooltip.rs`) — a drop-in wrapper that shows a second element on hover with zero new app state. This plan uses that built-in widget instead. Same user-facing behavior (hover a status indicator → see the gate breakdown), far less code, no new `Message` variant or `App` field.

---

## Task 1: `SessionFields` bitset + `Session::merge_from`

**Files:**
- Modify: `crates/ninox-core/src/types.rs`
- Test: `crates/ninox-core/src/types.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub struct SessionFields(u16)` with `pub const` members `STATUS`, `PR_LINK`, `COST`, `CONTEXT`, `TERMINAL_AT`, `PID`, `WORKSPACE`, `MODEL`, `ALL`, `NONE`; `pub fn contains(self, other: Self) -> bool`; `impl std::ops::BitOr for SessionFields`; `impl Session { pub fn merge_from(&mut self, incoming: &Session, fields: SessionFields) }`. Note: `GATE` is deliberately added later, in Task 2, alongside the `GateStatus` type it flags — adding it here would require referencing `Session.gate_status` before that field exists.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `crates/ninox-core/src/types.rs` (create the `#[cfg(test)] mod tests` block if none exists yet in this file — check first with `grep -n "mod tests" crates/ninox-core/src/types.rs`; if absent, add a new block at end of file):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn base_session() -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r1".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some("/ws".into()),
            pid: Some(111), model: Some("m1".into()), context_tokens: Some(10),
            catalogue_path: None, context_used_pct: Some(1.0),
            context_total_tokens: Some(10), context_window_size: Some(200_000),
            claude_session_id: None, summary: None, terminal_at: None,
        }
    }

    #[test]
    fn merge_from_only_copies_flagged_fields() {
        let mut existing = base_session();
        let mut incoming = base_session();
        // Incoming carries a *stale* repo/pr fields (as if read before another
        // actor's write landed) but a fresh cost_usd.
        incoming.repo = "stale-repo".into();
        incoming.cost_usd = 42.0;

        existing.merge_from(&incoming, SessionFields::COST);

        assert_eq!(existing.cost_usd, 42.0, "flagged field must be copied");
        assert_eq!(existing.repo, "r1", "unflagged field must survive untouched");
    }

    #[test]
    fn merge_from_disjoint_updates_do_not_stomp_each_other() {
        // Simulates two out-of-order Event::SessionUpdated arrivals touching
        // disjoint fields — this is the regression test PR #57's fix lacked
        // at the general level.
        let mut state = base_session();

        let mut a = base_session();
        a.status = SessionStatus::PrOpen;
        a.pr_number = Some(7);
        state.merge_from(&a, SessionFields::STATUS | SessionFields::PR_LINK);

        let mut b = base_session(); // stale snapshot: still pr_number None
        b.cost_usd = 9.99;
        state.merge_from(&b, SessionFields::COST);

        assert!(matches!(state.status, SessionStatus::PrOpen), "A's status must survive B's arrival");
        assert_eq!(state.pr_number, Some(7), "A's pr_number must survive B's arrival");
        assert_eq!(state.cost_usd, 9.99, "B's cost_usd must still apply");
    }

    #[test]
    fn merge_from_all_replaces_the_whole_struct() {
        let mut existing = base_session();
        let mut incoming = base_session();
        incoming.name = "brand-new-name".into();
        incoming.pid = Some(999);

        existing.merge_from(&incoming, SessionFields::ALL);

        assert_eq!(existing.name, "brand-new-name");
        assert_eq!(existing.pid, Some(999));
    }

    #[test]
    fn session_fields_bitor_combines_flags() {
        let combined = SessionFields::STATUS | SessionFields::PR_LINK;
        assert!(combined.contains(SessionFields::STATUS));
        assert!(combined.contains(SessionFields::PR_LINK));
        assert!(!combined.contains(SessionFields::COST));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core merge_from -- --nocapture`
Expected: FAIL to compile — `SessionFields` and `Session::merge_from` don't exist yet.

- [ ] **Step 3: Implement `SessionFields` and `Session::merge_from`**

Add to `crates/ninox-core/src/types.rs`, directly after the `Session` struct's closing `}` (currently ending at line 95):

```rust
/// Which fields of a `Session` a particular `Event::SessionUpdated` carries
/// fresh, authoritative values for. Every producer of that event is read
/// from a DB snapshot taken at the start of its own tick — a snapshot that
/// can be stale for fields *other* actors are concurrently writing. Flagging
/// exactly the fields a given tick just persisted, and merging field-by-field
/// on the receiving end (`Session::merge_from`), means a stale snapshot can
/// never stomp a fresher value for a field it isn't authoritative for —
/// closing the class of bug fixed one field at a time in PR #57.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFields(u16);

impl SessionFields {
    pub const NONE:        Self = Self(0);
    pub const STATUS:      Self = Self(1 << 0);
    /// `pr_number`, `pr_id`, and `repo` travel together — they're only ever
    /// self-healed/adopted as a unit (see `poller.rs`'s repo/PR self-heal).
    pub const PR_LINK:     Self = Self(1 << 2);
    pub const COST:        Self = Self(1 << 3);
    /// `context_tokens`, `context_used_pct`, `context_total_tokens`,
    /// `context_window_size` — all sourced from the same usage/statusline
    /// snapshot, so they travel together too.
    pub const CONTEXT:     Self = Self(1 << 4);
    pub const TERMINAL_AT: Self = Self(1 << 5);
    pub const PID:         Self = Self(1 << 6);
    pub const WORKSPACE:   Self = Self(1 << 7);
    pub const MODEL:       Self = Self(1 << 8);
    /// Full-struct replace — only for the spawn-completion event, where the
    /// row is transitioning from an optimistic placeholder to its first real
    /// snapshot and every field is being established for the first time.
    pub const ALL:         Self = Self(0xFFFF);
    // NOTE: bit `1 << 1` is intentionally left free — Task 2 adds a `GATE`
    // constant there, alongside the `GateStatus` type and the corresponding
    // `merge_from` branch, since both need `Session.gate_status` to exist.

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for SessionFields {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl Session {
    /// Copy only the fields flagged in `fields` from `incoming` onto `self`.
    /// See `SessionFields`'s doc comment for why this must never be a
    /// wholesale replace except when `fields == SessionFields::ALL`.
    pub fn merge_from(&mut self, incoming: &Session, fields: SessionFields) {
        if fields == SessionFields::ALL {
            *self = incoming.clone();
            return;
        }
        if fields.contains(SessionFields::STATUS) {
            self.status = incoming.status.clone();
        }
        // NOTE: a `GATE` branch is added here in Task 2, once
        // `Session.gate_status` exists.
        if fields.contains(SessionFields::PR_LINK) {
            self.pr_number = incoming.pr_number;
            self.pr_id = incoming.pr_id;
            self.repo = incoming.repo.clone();
        }
        if fields.contains(SessionFields::COST) {
            self.cost_usd = incoming.cost_usd;
        }
        if fields.contains(SessionFields::CONTEXT) {
            self.context_tokens = incoming.context_tokens;
            self.context_used_pct = incoming.context_used_pct;
            self.context_total_tokens = incoming.context_total_tokens;
            self.context_window_size = incoming.context_window_size;
        }
        if fields.contains(SessionFields::TERMINAL_AT) {
            self.terminal_at = incoming.terminal_at;
        }
        if fields.contains(SessionFields::PID) {
            self.pid = incoming.pid;
        }
        if fields.contains(SessionFields::WORKSPACE) {
            self.workspace_path = incoming.workspace_path.clone();
        }
        if fields.contains(SessionFields::MODEL) {
            self.model = incoming.model.clone();
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core merge_from`
Expected: PASS (4 tests: `merge_from_only_copies_flagged_fields`, `merge_from_disjoint_updates_do_not_stomp_each_other`, `merge_from_all_replaces_the_whole_struct`, `session_fields_bitor_combines_flags`).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/types.rs
git commit -m "feat(core): add SessionFields bitset and Session::merge_from"
```

---

## Task 2: `GateCheck`/`GateStatus` types + store column + round-trip

**Files:**
- Modify: `crates/ninox-core/src/types.rs`
- Modify: `crates/ninox-core/src/store.rs`
- Test: `crates/ninox-core/src/store.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SessionFields` from Task 1 (adds the `GATE` constant and its `merge_from` branch, both left out of Task 1 since they need `Session.gate_status` to exist).
- Produces: `pub enum GateCheck { Passing, Failing, Pending, Unknown }`; `pub struct GateStatus { pub ci: GateCheck, pub review: GateCheck, pub mergeable: GateCheck, pub since: i64 }`; `Session.gate_status: Option<GateStatus>`; `SessionFields::GATE`; `Store::upsert_session`/`list_sessions`/`get_session` all read/write `gate_status`.

- [ ] **Step 1: Write the failing test**

Add to `crates/ninox-core/src/store.rs`'s existing `#[cfg(test)] mod tests` block (find it via `grep -n "mod tests" crates/ninox-core/src/store.rs`; add alongside the existing session round-trip tests):

```rust
    #[test]
    fn upsert_and_fetch_session_round_trips_gate_status() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let mut session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1), workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None,
        };
        session.gate_status = Some(crate::types::GateStatus {
            ci: crate::types::GateCheck::Failing,
            review: crate::types::GateCheck::Passing,
            mergeable: crate::types::GateCheck::Unknown,
            since: 12_345,
        });
        store.upsert_session(&session).unwrap();

        let fetched = store.get_session("s1").unwrap().unwrap();
        assert_eq!(fetched.gate_status, session.gate_status);

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed[0].gate_status, session.gate_status);
    }

    #[test]
    fn legacy_row_without_gate_status_column_defaults_to_none() {
        // A row written before this migration has no gate_status column
        // value — column_exists-gated ALTER TABLE means the column is added
        // but existing rows get SQL NULL, which must deserialize to `None`.
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let session = crate::types::Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None,
        };
        store.upsert_session(&session).unwrap();
        let fetched = store.get_session("s2").unwrap().unwrap();
        assert_eq!(fetched.gate_status, None);
    }
```

Also add this test to `crates/ninox-core/src/types.rs`'s test module (alongside the `merge_from_*` tests from Task 1), covering the `GATE` flag this task adds:

```rust
    #[test]
    fn merge_from_gate_copies_only_when_flagged() {
        let mut existing = base_session();
        let mut incoming = base_session();
        incoming.gate_status = Some(GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Passing,
            mergeable: GateCheck::Unknown, since: 42,
        });

        existing.merge_from(&incoming, SessionFields::COST); // GATE not flagged
        assert_eq!(existing.gate_status, None, "unflagged GATE must not be copied");

        existing.merge_from(&incoming, SessionFields::GATE);
        assert_eq!(existing.gate_status, incoming.gate_status, "flagged GATE must be copied");
    }
```

(`base_session()`, from Task 1, needs `gate_status: None,` added to its literal — this is exactly the fix already required by Step 3's "every `Session { ... }` literal" sweep below, since `base_session()` lives in the same file.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core gate_status -- --nocapture`
Expected: FAIL to compile — `GateCheck`, `GateStatus`, `Session.gate_status`, and `SessionFields::GATE` don't exist yet.

- [ ] **Step 3: Add the types and wire up `Session`**

In `crates/ninox-core/src/types.rs`, add directly above the `Session` struct definition:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GateCheck {
    Passing,
    Failing,
    Pending,
    Unknown,
}

/// Structured snapshot of the three raw signals `derive_session_status`
/// already collapses into one `SessionStatus` — kept separately so the UI
/// can explain *which* check is blocking and *since when*, not just the
/// single derived enum value. Current-state only: no transition history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateStatus {
    pub ci:        GateCheck,
    pub review:    GateCheck,
    pub mergeable: GateCheck,
    /// Epoch ms this exact (ci, review, mergeable) combination was first
    /// observed — reset whenever any of the three values changes.
    pub since: i64,
}
```

Then add the new field to `Session` (after `terminal_at`, at the end of the struct):

```rust
    /// Structured CI/review/mergeable breakdown behind the current
    /// `status`. `None` until the first GitHub enrichment tick for a
    /// session with an open PR (`Spawning`/`Working` sessions have no PR
    /// yet, so no gate to report). `#[serde(default)]` for wire/DB
    /// back-compat with sessions recorded before this field existed.
    #[serde(default)]
    pub gate_status: Option<GateStatus>,
```

Add the `GATE` constant to `SessionFields` (Task 1 left the `1 << 1` bit free for exactly this):

```rust
    pub const GATE: Self = Self(1 << 1);
```

Add the corresponding branch to `Session::merge_from`, right after the existing `STATUS` branch (where Task 1 left a `NOTE` comment marking the spot):

```rust
        if fields.contains(SessionFields::GATE) {
            self.gate_status = incoming.gate_status.clone();
        }
```

Every existing literal `Session { ... }` construction elsewhere in the codebase (test helpers in `events.rs`, `poller.rs`, `app.rs`, `spawn_util.rs`, and this file's own `base_session()` from Task 1) now fails to compile with "missing field `gate_status`". Fix each by adding `gate_status: None,` to the struct literal. Find every site with:

```bash
grep -rln "Session {" crates/ | xargs grep -L "gate_status"
```

For each file returned, add `gate_status: None,` to every `Session { ... }` literal (do this now, even though most of these files aren't otherwise touched until later tasks — the crate must compile at the end of this task).

- [ ] **Step 4: Add the DB column and round-trip in `store.rs`**

In `crates/ninox-core/src/store.rs`, add to the migration list (after `("terminal_at", ...)`):

```rust
            ("gate_status",          "ALTER TABLE sessions ADD COLUMN gate_status TEXT"),
```

In `upsert_session`, add `gate_status` as a 22nd column — JSON-encoded, same pattern as `status`:

```rust
    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let gate_status = s.gate_status.as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
             ON CONFLICT(id) DO UPDATE SET
             repo=excluded.repo,
             status=excluded.status,cost_usd=excluded.cost_usd,
             started_at=excluded.started_at,
             pr_number=excluded.pr_number,pr_id=excluded.pr_id,
             workspace_path=excluded.workspace_path,pid=excluded.pid,
             model=excluded.model,context_tokens=excluded.context_tokens,
             catalogue_path=excluded.catalogue_path,
             context_used_pct=excluded.context_used_pct,
             context_total_tokens=excluded.context_total_tokens,
             context_window_size=excluded.context_window_size,
             claude_session_id=excluded.claude_session_id,
             summary=excluded.summary,
             terminal_at=excluded.terminal_at,
             gate_status=excluded.gate_status",
            params![
                s.id, s.orchestrator_id, s.name, s.repo, status, s.agent_type,
                s.cost_usd, s.started_at, s.pr_number, s.pr_id,
                s.workspace_path, s.pid, s.model, s.context_tokens,
                s.catalogue_path, s.context_used_pct, s.context_total_tokens,
                s.context_window_size, s.claude_session_id, s.summary, s.terminal_at,
                gate_status
            ],
        )?;
        Ok(())
    }
```

In `list_sessions`, add the column to the `SELECT`, the tuple `query_map` closure, the destructuring `let`, and the `Session { .. }` construction:

```rust
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status
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
                r.get::<_, Option<String>>(18)?,
                r.get::<_, Option<String>>(19)?,
                r.get::<_, Option<i64>>(20)?,
                r.get::<_, Option<String>>(21)?,
            ))
        })?;
        rows.map(|r| {
            let (id, orchestrator_id, name, repo, status_str, agent_type,
                 cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                 model, context_tokens, catalogue_path, context_used_pct,
                 context_total_tokens, context_window_size, claude_session_id,
                 summary, terminal_at, gate_status_str) = r?;
            let status = serde_json::from_str(&format!("\"{status_str}\""))
                .unwrap_or(SessionStatus::Working);
            let gate_status = gate_status_str
                .and_then(|s| serde_json::from_str(&s).ok());
            Ok(Session {
                id, orchestrator_id, name, repo, status, agent_type,
                cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                catalogue_path,
                context_used_pct,
                context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                context_window_size: context_window_size.map(|v| v.max(0) as u64),
                claude_session_id,
                summary,
                terminal_at,
                gate_status,
            })
        })
        .collect()
    }
```

Apply the identical pattern (SELECT column, tuple slot, destructure, `Session { .. }` field) to `get_session`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox-core gate_status`
Expected: PASS (`upsert_and_fetch_session_round_trips_gate_status`, `legacy_row_without_gate_status_column_defaults_to_none`, `merge_from_gate_copies_only_when_flagged`).

Run: `cargo build --workspace` to confirm every other `Session { .. }` literal fixed in Step 3 now compiles.
Expected: builds cleanly, no "missing field `gate_status`" errors.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/types.rs crates/ninox-core/src/store.rs
git commit -m "feat(core): add GateStatus type and persist it on Session"
```

---

## Task 3: Race-safe `Event::SessionUpdated` — ninox-core producer sites

**Files:**
- Modify: `crates/ninox-core/src/events.rs`
- Modify: `crates/ninox-core/src/lifecycle/poller.rs`

**Interfaces:**
- Consumes: `SessionFields` (Task 1).
- Produces: `Event::SessionUpdated(Session, SessionFields)` (signature change from `Event::SessionUpdated(Session)`).

- [ ] **Step 1: Change the `Event` enum variant**

In `crates/ninox-core/src/events.rs`, change:

```rust
    SessionUpdated(Session),
```

to:

```rust
    SessionUpdated(Session, SessionFields),
```

Add `SessionFields` to this file's `use` of `crate::types::*` (already a glob import per `events.rs`'s existing `use crate::types::*;` — confirm with `grep -n "^use" crates/ninox-core/src/events.rs`; no import change needed if it's already a glob).

- [ ] **Step 2: Fix every producer call site in `events.rs`**

`terminate_session` (currently emits at what's `events.rs:185`) — authoritative for `status` only (explicitly must NOT touch `terminal_at`, per the existing doc comment on this function):

```rust
            session.status = SessionStatus::Terminated;
            self.store.upsert_session(&session)?;
            self.emit(Event::SessionUpdated(session, SessionFields::STATUS));
```

`cleanup_session_in` (currently emits at what's `events.rs:215`) — authoritative for `status` and `terminal_at`:

```rust
            session.status = crate::types::SessionStatus::Done;
            session.terminal_at = Some(crate::lifecycle::poller::now_millis());
            self.store.upsert_session(&session)?;
            self.emit(Event::SessionUpdated(session, SessionFields::STATUS | SessionFields::TERMINAL_AT));
```

- [ ] **Step 3: Fix the two existing tests in `events.rs` that destructure `Event::SessionUpdated`**

```rust
        let evt = rx.recv().await.unwrap();
        if let Event::SessionUpdated(s, _fields) = evt {
```

(Apply to both occurrences — the `terminate_session` test and the `cleanup_session_sets_done_status` test.)

- [ ] **Step 4: Fix every producer call site in `poller.rs`**

`poll_pids` (dead-process detection) — `status` + `terminal_at`:

```rust
                if !is_pid_alive(pid) {
                    session.status = SessionStatus::Terminated;
                    session.terminal_at = Some(now_millis());
                    let _ = self.engine.store.upsert_session(&session);
                    self.engine.emit(Event::SessionUpdated(session, SessionFields::STATUS | SessionFields::TERMINAL_AT));
                }
```

`sync_sessions_metadata` (PR adoption from the self-report hook) — `pr_number`/`status` (part of `PR_LINK` + `STATUS`; `pr_id`/`repo` are untouched here, but `PR_LINK` copying them as a unit from a fresh-this-tick `session` clone is safe — they weren't touched this tick, so `incoming`'s values for them equal whatever was already correct):

```rust
                if session.pr_number.is_none() {
                    if let Some(first) = meta.pr_reports.first() {
                        session.pr_number = Some(first.number);
                        session.status    = SessionStatus::PrOpen;
                        let _ = self.engine.store.upsert_session(&session);
                        self.engine.emit(Event::SessionUpdated(
                            session.clone(), SessionFields::PR_LINK | SessionFields::STATUS,
                        ));
```

`poll_usage` — `cost_usd`/context fields/`model`:

```rust
            session.cost_usd = snapshot.cost_usd;
            session.context_tokens = Some(snapshot.context_tokens);
            if session.model.is_none() {
                session.model = snapshot.model;
            }
            let _ = self.engine.store.upsert_session(&session);
            self.engine.emit(Event::SessionUpdated(
                session, SessionFields::COST | SessionFields::CONTEXT | SessionFields::MODEL,
            ));
```

`poll_context_updates` (statusline-sourced) — `cost_usd`/context fields (no `upsert_session` call at this site — it only detects+rebroadcasts a change another process already wrote):

```rust
        for session in changed {
            self.engine.emit(Event::SessionUpdated(
                session, SessionFields::COST | SessionFields::CONTEXT,
            ));
        }
```

`poll_github`'s status/gate write — `status`/`pr_number`/`pr_id`/`repo` (the self-heal a few lines earlier in the same tick already persisted `repo`/`pr_number`/`pr_id`, so it's safe to flag `PR_LINK` here too; `GATE` is added in Task 5, not this task):

```rust
            let new_status = derive_session_status(&session.status, &pr_status, &ci, has_changes_requested);
            let mut updated = session.clone();
            updated.status = new_status;
            if updated.status != session.status {
                let _ = self.engine.store.upsert_session(&updated);
                self.engine.emit(Event::SessionUpdated(
                    updated.clone(), SessionFields::STATUS | SessionFields::PR_LINK,
                ));
            }
```

`poll_pr_reconciliation` — `pr_number`/`repo`/`status`:

```rust
                    Ok(Some(pr_ref)) => {
                        session.pr_number = Some(pr_ref.number);
                        session.repo      = repo_slug.clone();
                        session.status    = SessionStatus::PrOpen;
                        let _ = self.engine.store.upsert_session(&session);
                        self.engine.emit(Event::SessionUpdated(
                            session.clone(), SessionFields::PR_LINK | SessionFields::STATUS,
                        ));
```

- [ ] **Step 5: Fix the two existing tests in `poller.rs` that destructure `Event::SessionUpdated`**

```rust
        assert!(matches!(evt, Event::SessionUpdated(s, _fields) if s.id == "s1" && s.cost_usd > 0.0));
```

and

```rust
            Event::SessionUpdated(s, _fields) if s.id == "s1" && s.context_used_pct == Some(42.0) && s.cost_usd == 3.5
```

- [ ] **Step 6: Fix `ninox-server`'s SSE serialization match arm**

In `crates/ninox-server/src/routes/events.rs`:

```rust
        Event::SessionUpdated(s, _fields) => serde_json::json!({"type": "session_updated", "payload": s}),
```

- [ ] **Step 7: Build and run the full `ninox-core`/`ninox-server` test suites**

Run: `cargo build -p ninox-core -p ninox-server`
Expected: fails only on `ninox-app`'s not-yet-updated call sites (Task 4) — `ninox-core`/`ninox-server` themselves must build clean. If `ninox-app` errors block the workspace build, use `cargo build -p ninox-core -p ninox-server` explicitly rather than `cargo build --workspace`.

Run: `cargo test -p ninox-core -p ninox-server`
Expected: PASS, including the pre-existing tests updated in Steps 3 and 5.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-core/src/events.rs crates/ninox-core/src/lifecycle/poller.rs crates/ninox-server/src/routes/events.rs
git commit -m "feat(core): tag Event::SessionUpdated with the fields each tick owns"
```

---

## Task 4: Race-safe `Event::SessionUpdated` — ninox-app merge-on-apply and remaining producers

**Files:**
- Modify: `crates/ninox-app/src/app.rs`
- Modify: `crates/ninox-app/src/spawn_util.rs`

**Interfaces:**
- Consumes: `Event::SessionUpdated(Session, SessionFields)` (Task 3), `Session::merge_from` (Task 1).
- Produces: `App`'s event-apply path now merges instead of replacing.

- [ ] **Step 1: Write the failing test**

Add to `crates/ninox-app/src/app.rs`'s existing `#[cfg(test)] mod tests` block (there's an extensive one already — find a session-related test for the pattern to match, e.g. search `grep -n "fn spawning_a_session\|SessionSpawned" crates/ninox-app/src/app.rs` for a model to copy setup from):

```rust
    #[test]
    fn session_updated_merges_instead_of_replacing() {
        let (mut app, _config_dir) = test_app();
        let mut session = ninox_core::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r1".into(), status: ninox_core::types::SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
        };
        let (updated, _) = app.update(Message::EngineEvent(Box::new(
            Event::SessionSpawned(session.clone()),
        )));
        app = updated;

        // Actor A: authoritative for PR_LINK + STATUS, from a fresh read.
        session.status = ninox_core::types::SessionStatus::PrOpen;
        session.pr_number = Some(7);
        let (after_a, _) = app.update(Message::EngineEvent(Box::new(
            Event::SessionUpdated(session.clone(), SessionFields::STATUS | SessionFields::PR_LINK),
        )));
        app = after_a;

        // Actor B: authoritative for COST only, from a *stale* read that
        // still has pr_number: None — must not stomp A's PR_LINK write.
        let mut stale = session.clone();
        stale.pr_number = None;
        stale.status = ninox_core::types::SessionStatus::Working;
        stale.cost_usd = 3.25;
        let (after_b, _) = app.update(Message::EngineEvent(Box::new(
            Event::SessionUpdated(stale, SessionFields::COST),
        )));

        let final_session = after_b.sessions.get("s1").unwrap();
        assert_eq!(final_session.cost_usd, 3.25, "B's flagged field must apply");
        assert_eq!(final_session.pr_number, Some(7), "A's pr_number must survive B's stale rebroadcast");
        assert!(matches!(final_session.status, ninox_core::types::SessionStatus::PrOpen), "A's status must survive B's stale rebroadcast");
    }
```

(If a `test_app()` helper doesn't already exist, check the nearby tests for the actual construction pattern used — e.g. `grep -n "fn test_app\|App::new\|fn m()" crates/ninox-app/src/app.rs | head -20` — and use that exact pattern instead; every other test in this file already constructs an `App` somehow, so mirror the one immediately above/below where you insert this test rather than inventing a new setup path.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox merges_instead_of_replacing -- --nocapture`
Expected: FAIL — either a compile error (old `Event::SessionUpdated(Session)` shape from Task 3 changing the arity) or, once compiling, an assertion failure showing `pr_number` reset to `None`/`status` reset to `Working` (today's wholesale-replace behavior).

- [ ] **Step 3: Fix the apply-path handler**

In `crates/ninox-app/src/app.rs`, change:

```rust
            Event::SessionUpdated(session) => {
                state.sessions.insert(session.id.clone(), session);
                Task::none()
            }
```

to:

```rust
            Event::SessionUpdated(incoming, fields) => {
                match state.sessions.get_mut(&incoming.id) {
                    Some(existing) => existing.merge_from(&incoming, fields),
                    None => { state.sessions.insert(incoming.id.clone(), incoming); }
                }
                Task::none()
            }
```

- [ ] **Step 4: Fix the three remaining `ninox-app` producer call sites**

Startup reconciliation (`app.rs`, dead-tmux-on-launch path) — `status` only:

```rust
                    let _ = engine.store.upsert_session(&dead);
                    engine.emit(CoreEvent::SessionUpdated(dead, ninox_core::types::SessionFields::STATUS));
```

Session-detail attach (`app.rs`, dead-tmux-on-navigate path) — `status` only:

```rust
                        if let Ok(Some(mut s)) = engine.store.get_session(&id) {
                            s.status = ninox_core::types::SessionStatus::Terminated;
                            let _ = engine.store.upsert_session(&s);
                            engine.emit(ninox_core::events::Event::SessionUpdated(
                                s, ninox_core::types::SessionFields::STATUS,
                            ));
                        }
```

`spawn_util.rs`'s tmux-create-failure path — `status` only:

```rust
        if let Ok(Some(mut s)) = engine.store.get_session(&sid) {
            s.status = p.failure_status;
            let _ = engine.store.upsert_session(&s);
            engine.emit(Event::SessionUpdated(s, SessionFields::STATUS));
        }
```

`spawn_util.rs`'s spawn-success path — full replace (the row is completing its first real snapshot after the optimistic placeholder insert):

```rust
    let _ = engine.store.upsert_session(&updated);

    if let Err(e) = pty::start_streaming(engine.clone(), sid.clone(), &sid).await {
        tracing::error!("pty setup failed for {sid}: {e}");
    }

    engine.emit(Event::SessionUpdated(updated, SessionFields::ALL));
```

(Add `SessionFields` to `spawn_util.rs`'s existing `use ninox_core::...` import if it isn't already covered by a glob — check with `grep -n "^use" crates/ninox-app/src/spawn_util.rs`.)

- [ ] **Step 5: Run the new test and the full `ninox-app` suite**

Run: `cargo test -p ninox session_updated_merges_instead_of_replacing`
Expected: PASS.

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds and passes clean across all three crates — this is the first point since Task 3 Step 1 that the whole workspace compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src/app.rs crates/ninox-app/src/spawn_util.rs
git commit -m "fix(app): merge SessionUpdated fields instead of replacing the whole session"
```

---

## Task 5: Compute and persist `GateStatus` in `poll_github`

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/poller.rs`

**Interfaces:**
- Consumes: `GateStatus`/`GateCheck` (Task 2), `SessionFields::GATE` (Tasks 1 & 3).
- Produces: `fn derive_gate_status(ci: &CIStatus, has_changes_requested: bool, mergeable: Option<bool>, previous: Option<&GateStatus>, now: i64) -> GateStatus`.

- [ ] **Step 1: Write the failing tests**

Add to `poller.rs`'s existing `#[cfg(test)] mod tests` block, alongside `derive_session_status`'s tests (found via `grep -n "fn derive_status_preserves_done" crates/ninox-core/src/lifecycle/poller.rs`):

```rust
    #[test]
    fn derive_gate_status_maps_raw_signals() {
        let ci = CIStatus { pr_id: 1, total: 3, passing: 1, failing: 1, pending: 1 };
        let gate = derive_gate_status(&ci, true, Some(false), None, 1_000);
        assert!(matches!(gate.ci, GateCheck::Failing));
        assert!(matches!(gate.review, GateCheck::Failing));
        assert!(matches!(gate.mergeable, GateCheck::Failing));
        assert_eq!(gate.since, 1_000, "first observation stamps `since` to `now`");
    }

    #[test]
    fn derive_gate_status_maps_all_passing() {
        let ci = CIStatus { pr_id: 1, total: 2, passing: 2, failing: 0, pending: 0 };
        let gate = derive_gate_status(&ci, false, Some(true), None, 1_000);
        assert!(matches!(gate.ci, GateCheck::Passing));
        assert!(matches!(gate.review, GateCheck::Passing));
        assert!(matches!(gate.mergeable, GateCheck::Passing));
    }

    #[test]
    fn derive_gate_status_maps_pending_ci_and_unknown_mergeable() {
        let ci = CIStatus { pr_id: 1, total: 2, passing: 0, failing: 0, pending: 2 };
        let gate = derive_gate_status(&ci, false, None, None, 1_000);
        assert!(matches!(gate.ci, GateCheck::Pending));
        assert!(matches!(gate.mergeable, GateCheck::Unknown));
    }

    #[test]
    fn derive_gate_status_carries_since_forward_when_unchanged() {
        let ci = CIStatus { pr_id: 1, total: 1, passing: 1, failing: 0, pending: 0 };
        let previous = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Passing,
            mergeable: GateCheck::Passing, since: 500,
        };
        let gate = derive_gate_status(&ci, false, Some(true), Some(&previous), 9_999);
        assert_eq!(gate.since, 500, "unchanged combination keeps the original `since`");
    }

    #[test]
    fn derive_gate_status_resets_since_when_combination_changes() {
        let ci = CIStatus { pr_id: 1, total: 1, passing: 0, failing: 1, pending: 0 };
        let previous = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Passing,
            mergeable: GateCheck::Passing, since: 500,
        };
        let gate = derive_gate_status(&ci, false, Some(true), Some(&previous), 9_999);
        assert!(matches!(gate.ci, GateCheck::Failing));
        assert_eq!(gate.since, 9_999, "a changed combination resets `since` to `now`");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core derive_gate_status -- --nocapture`
Expected: FAIL to compile — `derive_gate_status` doesn't exist yet.

- [ ] **Step 3: Implement `derive_gate_status`**

Add directly after `derive_session_status` (currently ending at `poller.rs:972`):

```rust
fn derive_gate_status(
    ci:                    &CIStatus,
    has_changes_requested: bool,
    mergeable:             Option<bool>,
    previous:              Option<&GateStatus>,
    now:                   i64,
) -> GateStatus {
    let ci_check = if ci.failing > 0 {
        GateCheck::Failing
    } else if ci.pending > 0 {
        GateCheck::Pending
    } else {
        GateCheck::Passing
    };
    let review_check = if has_changes_requested { GateCheck::Failing } else { GateCheck::Passing };
    let mergeable_check = match mergeable {
        Some(true)  => GateCheck::Passing,
        Some(false) => GateCheck::Failing,
        None        => GateCheck::Unknown,
    };

    let unchanged = previous.is_some_and(|p| {
        p.ci == ci_check && p.review == review_check && p.mergeable == mergeable_check
    });
    let since = if unchanged { previous.unwrap().since } else { now };

    GateStatus { ci: ci_check, review: review_check, mergeable: mergeable_check, since }
}
```

`GateCheck` needs `PartialEq` for the `unchanged` comparison above — already derived in Task 2's `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]`, so no further change needed there.

- [ ] **Step 4: Wire it into `poll_github`'s status-update write**

Change the block at (originally) `poller.rs:704-711`:

```rust
            // Update session status in DB (after review threads so has_changes_requested is known)
            let new_status = derive_session_status(&session.status, &pr_status, &ci, has_changes_requested);
            let new_gate = derive_gate_status(
                &ci, has_changes_requested, pr_status.mergeable, session.gate_status.as_ref(), now_millis(),
            );
            let mut updated = session.clone();
            updated.status = new_status;
            updated.gate_status = Some(new_gate);
            if updated.status != session.status || updated.gate_status != session.gate_status {
                let _ = self.engine.store.upsert_session(&updated);
                self.engine.emit(Event::SessionUpdated(
                    updated.clone(),
                    SessionFields::STATUS | SessionFields::PR_LINK | SessionFields::GATE,
                ));
            }
```

(Note the changed guard: previously only `updated.status != session.status` triggered a write; now a gate-only change — e.g. CI flips from pending to passing while status stays `Mergeable` — also triggers a write, since that's exactly the kind of change the tooltip needs to reflect promptly.)

- [ ] **Step 5: Run the new tests and the full `derive_session_status` regression suite**

Run: `cargo test -p ninox-core derive_gate_status`
Expected: PASS (5 new tests).

Run: `cargo test -p ninox-core derive_session_status derive_status_preserves`
Expected: PASS, unchanged — confirms this task didn't disturb the existing status-derivation guards.

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds and passes clean.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/lifecycle/poller.rs
git commit -m "feat(core): compute and persist GateStatus alongside SessionStatus"
```

---

## Task 6: Gate tooltip — `lifecycle_status.rs` component + sidebar/fleet-board wiring

**Files:**
- Create: `crates/ninox-app/src/components/lifecycle_status.rs`
- Modify: `crates/ninox-app/src/components/mod.rs`
- Modify: `crates/ninox-app/src/components/sidebar.rs`
- Modify: `crates/ninox-app/src/components/fleet_board.rs`

**Interfaces:**
- Consumes: `Session.gate_status`/`Session.status` (Task 2), `ninox_core::types::{GateCheck, GateStatus}`.
- Produces: `pub fn gate_lines(session: &Session) -> Vec<String>`; `pub fn with_gate_tooltip<'a>(s: &ColorScheme, session: &'a Session, content: Element<'a, Message>) -> Element<'a, Message>`.

- [ ] **Step 1: Write the failing test**

Create `crates/ninox-app/src/components/lifecycle_status.rs` with just the test module first:

```rust
use ninox_core::types::{GateCheck, GateStatus, Session, SessionStatus};

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with(status: SessionStatus, gate: Option<GateStatus>) -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status, agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: None, model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: gate,
        }
    }

    #[test]
    fn gate_lines_before_any_pr_explains_no_gate_yet() {
        let session = session_with(SessionStatus::Working, None);
        let lines = gate_lines(&session);
        assert_eq!(lines, vec!["No PR opened yet".to_string()]);
    }

    #[test]
    fn gate_lines_renders_each_check_plainly() {
        let gate = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Failing,
            mergeable: GateCheck::Failing, since: 0,
        };
        let session = session_with(SessionStatus::ReviewPending, Some(gate));
        let lines = gate_lines(&session);
        assert_eq!(lines, vec![
            "CI — passing".to_string(),
            "Review — changes requested".to_string(),
            "Mergeable — blocked on review".to_string(),
        ]);
    }

    #[test]
    fn gate_lines_explains_ci_blocking_mergeable() {
        let gate = GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Passing,
            mergeable: GateCheck::Failing, since: 0,
        };
        let session = session_with(SessionStatus::CiFailed, Some(gate));
        let lines = gate_lines(&session);
        assert_eq!(lines, vec![
            "CI — failing".to_string(),
            "Review — approved".to_string(),
            "Mergeable — blocked on CI".to_string(),
        ]);
    }

    #[test]
    fn gate_lines_reports_passing_mergeable_directly() {
        let gate = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Passing,
            mergeable: GateCheck::Passing, since: 0,
        };
        let session = session_with(SessionStatus::Mergeable, Some(gate));
        let lines = gate_lines(&session);
        assert_eq!(lines, vec![
            "CI — passing".to_string(),
            "Review — approved".to_string(),
            "Mergeable — yes".to_string(),
        ]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox gate_lines -- --nocapture`
Expected: FAIL to compile — `gate_lines` doesn't exist yet.

- [ ] **Step 3: Implement `gate_lines`**

Add above the `#[cfg(test)]` block in `lifecycle_status.rs`:

```rust
/// Plain-English breakdown of a session's current gate state, one line per
/// check — what the hover tooltip shows. `Mergeable`'s line explains *why*
/// when it's blocked, rather than just repeating "no."
pub fn gate_lines(session: &Session) -> Vec<String> {
    let Some(gate) = &session.gate_status else {
        return vec!["No PR opened yet".to_string()];
    };

    let ci_word = match gate.ci {
        GateCheck::Passing => "passing",
        GateCheck::Failing => "failing",
        GateCheck::Pending => "pending",
        GateCheck::Unknown => "unknown",
    };
    let review_word = match gate.review {
        GateCheck::Passing => "approved",
        GateCheck::Failing => "changes requested",
        GateCheck::Pending => "pending",
        GateCheck::Unknown => "unknown",
    };
    let mergeable_line = match gate.mergeable {
        GateCheck::Passing => "Mergeable — yes".to_string(),
        GateCheck::Unknown => "Mergeable — unknown".to_string(),
        GateCheck::Pending => "Mergeable — pending".to_string(),
        GateCheck::Failing => {
            if matches!(gate.ci, GateCheck::Failing) {
                "Mergeable — blocked on CI".to_string()
            } else if matches!(gate.review, GateCheck::Failing) {
                "Mergeable — blocked on review".to_string()
            } else {
                "Mergeable — no".to_string()
            }
        }
    };

    vec![
        format!("CI — {ci_word}"),
        format!("Review — {review_word}"),
        mergeable_line,
    ]
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox gate_lines`
Expected: PASS (4 tests).

- [ ] **Step 5: Add `with_gate_tooltip` and register the module**

Add to `crates/ninox-app/src/components/mod.rs` (alphabetically, between `inspector_panel` and `links`):

```rust
pub mod lifecycle_status;
```

Add to the top of `lifecycle_status.rs` and a new function below `gate_lines`:

```rust
use iced::{
    widget::{column, container, text, tooltip, Space},
    Background, Border, Element, Length,
};

use crate::{app::Message, style::shadow_alpha, theme::ColorScheme};

/// Wraps `content` (typically a status dot/stamp) so hovering it shows a
/// plain-English gate breakdown — reuses `brain_panel.rs`'s hover-slip
/// styling (paper_2 background, ink border, hard drop shadow) but via
/// Iced's built-in `tooltip` widget rather than manual hover-state
/// tracking, since these rows are ordinary widget trees (not a canvas).
pub fn with_gate_tooltip<'a>(
    s:       &ColorScheme,
    session: &'a ninox_core::types::Session,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    let (card_a, _, _) = shadow_alpha(s);
    let lines: Vec<Element<Message>> = gate_lines(session)
        .into_iter()
        .map(|line| text(line).size(11).font(crate::style::SANS).color(s.ink_2).into())
        .collect();

    let body = container(column(lines).spacing(3))
        .width(Length::Fixed(200.0))
        .padding([10, 12])
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.paper_2)),
            border: Border { color: s.ink, width: 1.5, radius: 2.0.into() },
            shadow: crate::style::hard_shadow(s, 3.0, 3.0, card_a),
            ..Default::default()
        });

    tooltip(content, body, tooltip::Position::Bottom)
        .gap(6)
        .into()
}
```

- [ ] **Step 6: Wire into `sidebar.rs`'s `tree_row`**

In `crates/ninox-app/src/components/sidebar.rs`, `tree_row` currently builds `dot` and drops it straight into the `navigate` button's `row!`. Change:

```rust
    let dot: Element<Message> = match status {
        Some(st) => status_dot(
            s.status_color(st),
            matches!(st, ninox_core::types::SessionStatus::Done
                        | ninox_core::types::SessionStatus::Terminated),
        ),
        None => Space::new(8, 0).into(),
    };
```

to:

```rust
    let dot: Element<Message> = match status {
        Some(st) => {
            let base = status_dot(
                s.status_color(st),
                matches!(st, ninox_core::types::SessionStatus::Done
                            | ninox_core::types::SessionStatus::Terminated),
            );
            match app.sessions.get(id) {
                Some(session) => crate::components::lifecycle_status::with_gate_tooltip(s, session, base),
                None => base,
            }
        }
        None => Space::new(8, 0).into(),
    };
```

- [ ] **Step 7: Wire into `fleet_board.rs`'s `session_card`**

In `crates/ninox-app/src/components/fleet_board.rs`, `session_card` currently builds `crate::style::stamp(word, st_color)` inline inside the `row!` that also shows cost. Change:

```rust
    body.push(
        row![
            crate::style::stamp(word, st_color),
            Space::new(Length::Fill, 0),
            text(format!("${:.2}", session.cost_usd))
                .size(11.5).font(crate::style::MONO_MEDIUM).color(s.ink),
        ]
        .align_y(Alignment::Center)
        .into(),
    );
```

to:

```rust
    let stamp_with_tooltip = crate::components::lifecycle_status::with_gate_tooltip(
        s, session, crate::style::stamp(word, st_color),
    );
    body.push(
        row![
            stamp_with_tooltip,
            Space::new(Length::Fill, 0),
            text(format!("${:.2}", session.cost_usd))
                .size(11.5).font(crate::style::MONO_MEDIUM).color(s.ink),
        ]
        .align_y(Alignment::Center)
        .into(),
    );
```

- [ ] **Step 8: Build and manually sanity-check**

Run: `cargo build --workspace`
Expected: builds clean.

Run: `cargo test --workspace`
Expected: PASS, no regressions.

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-app/src/components/lifecycle_status.rs crates/ninox-app/src/components/mod.rs crates/ninox-app/src/components/sidebar.rs crates/ninox-app/src/components/fleet_board.rs
git commit -m "feat(app): add gate-status hover tooltip to sidebar and fleet board"
```

---

## Task 7: Gate tooltip on PR list + retention badge

**Files:**
- Modify: `crates/ninox-app/src/components/pr_list.rs`
- Modify: `crates/ninox-app/src/components/lifecycle_status.rs`
- Modify: `crates/ninox-app/src/components/sidebar.rs`
- Modify: `crates/ninox-app/src/components/fleet_board.rs`

**Interfaces:**
- Consumes: `Session.terminal_at` (existing field), `SessionRetentionConfig` (existing, `ninox_core::config`), `with_gate_tooltip` (Task 6).
- Produces: `pub fn retention_label(terminal_at: i64, retention_millis: i64, now: i64) -> Option<String>`.

- [ ] **Step 1: Write the failing test for `retention_label`**

Add to `lifecycle_status.rs`'s existing test module:

```rust
    #[test]
    fn retention_label_shows_days_when_more_than_a_day_remains() {
        let label = retention_label(0, 2 * 86_400_000, 86_400_000 /* now = 1 day later */);
        assert_eq!(label, Some("Removing in 1d".to_string()));
    }

    #[test]
    fn retention_label_shows_hours_under_a_day() {
        let label = retention_label(0, 86_400_000, 68_400_000 /* now = 19h later, 5h left */);
        assert_eq!(label, Some("Removing in 5h".to_string()));
    }

    #[test]
    fn retention_label_shows_minutes_under_an_hour() {
        let label = retention_label(0, 3_600_000, 3_300_000 /* now = 55m later, 5m left */);
        assert_eq!(label, Some("Removing in 5m".to_string()));
    }

    #[test]
    fn retention_label_past_the_window_says_shortly() {
        let label = retention_label(0, 3_600_000, 3_700_000 /* now past the window */);
        assert_eq!(label, Some("Removing shortly".to_string()));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox retention_label -- --nocapture`
Expected: FAIL to compile — `retention_label` doesn't exist yet.

- [ ] **Step 3: Implement `retention_label` and `humanize_duration`**

Add to `lifecycle_status.rs`:

```rust
/// "Removing in Nd/Nh/Nm" for a terminal session sitting in its retention
/// grace period, or `None` if there's nothing to show (no `terminal_at`,
/// i.e. the session isn't terminal, or was terminated by direct user
/// action and has no grace period at all).
pub fn retention_label(terminal_at: i64, retention_millis: i64, now: i64) -> Option<String> {
    let remaining_ms = terminal_at + retention_millis - now;
    Some(if remaining_ms <= 0 {
        "Removing shortly".to_string()
    } else {
        format!("Removing in {}", humanize_duration(remaining_ms))
    })
}

/// Exactly one unit, the coarsest that's still >= 1: days, else hours,
/// else minutes.
fn humanize_duration(ms: i64) -> String {
    const MINUTE: i64 = 60_000;
    const HOUR:   i64 = 60 * MINUTE;
    const DAY:    i64 = 24 * HOUR;
    if ms >= DAY {
        format!("{}d", ms / DAY)
    } else if ms >= HOUR {
        format!("{}h", ms / HOUR)
    } else {
        format!("{}m", (ms / MINUTE).max(1))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox retention_label`
Expected: PASS (4 tests).

- [ ] **Step 5: Wire the badge into `sidebar.rs`'s `tree_row`**

`tree_row` needs `now` and the retention config to compute the label. `App.config: AppConfig` already carries `session_retention: SessionRetentionConfig` (`crates/ninox-core/src/config.rs:174`), so the config half is `app.config.session_retention.retention_millis()` — no new plumbing needed.

For "now," `ninox_core::lifecycle::poller::now_millis()` exists but is `pub(crate)` (`poller.rs:32`, deliberately scoped to `ninox-core` — its doc comment says so explicitly) and is not reachable from `ninox-app`. Rather than widen a core-internal helper's visibility for a UI display concern, add a private equivalent directly in `lifecycle_status.rs`:

```rust
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
```

Iced re-invokes `view()` on every redraw tick already, so calling this directly from the render function is enough for a coarse countdown — no new subscription/timer needed.

In `tree_row`, after computing `dot`, add:

```rust
    let retention_badge: Option<Element<Message>> = status
        .filter(|st| matches!(st, ninox_core::types::SessionStatus::Done | ninox_core::types::SessionStatus::Terminated))
        .and_then(|_| app.sessions.get(id))
        .and_then(|session| session.terminal_at)
        .and_then(|terminal_at| {
            crate::components::lifecycle_status::retention_label(
                terminal_at,
                app.config.session_retention.retention_millis(),
                now_millis(),
            )
        })
        .map(|label| {
            text(label).size(9.5).font(crate::style::MONO).color(s.faint).into()
        });
```

Then include it in the row, right after the `right` text element inside the `navigate` button's `row![...]` (append before the closing `]`):

```rust
            text(right.to_owned())
                .size(10)
                .font(MONO)
                .color(s.faint)
                .wrapping(iced::widget::text::Wrapping::None),
        ]
```

becomes (adding a conditional trailing element — since Iced's `row!` macro takes a fixed element list, switch this one row to the `row(Vec<Element>)` free-function form, which this same file already uses for `row_items` at `sidebar.rs:376`):

```rust
    let mut nav_row_items: Vec<Element<Message>> = vec![
        container(Space::new(0, 0)).width(3).height(Length::Fixed(20.0)).style(move |_| {
            container::Style {
                background: Some(Background::Color(if is_active { s.accent } else { Color::TRANSPARENT })),
                ..Default::default()
            }
        }).into(),
        Space::new(left_pad - 3.0, 0).into(),
        dot,
        Space::new(9, 0).into(),
        container(
            text(name.to_owned())
                .size(12.5)
                .font(name_font)
                .color(if is_active || bold { s.ink } else { s.ink_2 })
                .wrapping(iced::widget::text::Wrapping::None),
        )
        .width(Length::Fill)
        .clip(true)
        .into(),
        Space::new(6, 0).into(),
        text(right.to_owned())
            .size(10)
            .font(MONO)
            .color(s.faint)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
    ];
    if let Some(badge) = retention_badge {
        nav_row_items.push(Space::new(6, 0).into());
        nav_row_items.push(badge);
    }

    let navigate = button(
        row(nav_row_items).align_y(Alignment::Center),
    )
```

(This replaces the original `row![...]` literal passed to `button(...)` — remove the old literal entirely so there's exactly one row construction feeding `navigate`.)

- [ ] **Step 6: Wire the badge into `fleet_board.rs`'s `session_card`**

After the `stamp_with_tooltip` line added in Task 6 Step 7, add:

```rust
    let retention_badge: Option<Element<Message>> = if matches!(
        session.status, SessionStatus::Done | SessionStatus::Terminated
    ) {
        session.terminal_at.and_then(|terminal_at| {
            crate::components::lifecycle_status::retention_label(
                terminal_at,
                app.config.session_retention.retention_millis(),
                now_millis(),
            )
        }).map(|label| text(label).size(9.5).font(crate::style::MONO).color(s.faint).into())
    } else {
        None
    };
```

Then change the cost row to include it:

```rust
    let mut bottom_row: Vec<Element<Message>> = vec![
        stamp_with_tooltip,
        Space::new(Length::Fill, 0).into(),
        text(format!("${:.2}", session.cost_usd))
            .size(11.5).font(crate::style::MONO_MEDIUM).color(s.ink).into(),
    ];
    if let Some(badge) = retention_badge {
        bottom_row.insert(1, badge);
        bottom_row.insert(2, Space::new(8, 0).into());
    }
    body.push(row(bottom_row).align_y(Alignment::Center).into());
```

(Remove the earlier `body.push(row![stamp_with_tooltip, ...].into())` statement from Task 6 Step 7 — this replaces it.)

- [ ] **Step 7: Wire the tooltip into `pr_list.rs`'s status dot**

In `pr_list.rs`'s `pr_row`, change:

```rust
            container(
                row![
                    status_dot(session_color),
                    Space::new(7, 0),
                    text(session_name).size(11.5).font(SANS).color(s.ink_2),
                ]
                .align_y(Alignment::Center),
            )
            .width(Length::Fixed(150.0)),
```

to:

```rust
            container(
                row![
                    match session {
                        Some(se) => crate::components::lifecycle_status::with_gate_tooltip(
                            s, se, status_dot(session_color),
                        ),
                        None => status_dot(session_color),
                    },
                    Space::new(7, 0),
                    text(session_name).size(11.5).font(SANS).color(s.ink_2),
                ]
                .align_y(Alignment::Center),
            )
            .width(Length::Fixed(150.0)),
```

- [ ] **Step 8: Build, run the full suite, and manually verify in the running app**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds and passes clean.

Run: `cargo run -p ninox` (or the project's existing dev-run command — check `justfile` for a `run`/`dev` recipe first with `grep -n "^run\|^dev" justfile`), then:
1. Spawn or find a session with an open PR. Hover its status dot in the sidebar, the fleet board card's stamp, and (if the PR appears there) the PR list row. Confirm the same plain-English gate breakdown appears in all three places and matches the session's actual CI/review/mergeable state (cross-check against the GitHub PR page).
2. Find or produce a `Done`/`Terminated` session (merge a test PR, or wait for a worker's process to exit naturally). Confirm a "Removing in _N_" badge appears next to it in both the sidebar and the fleet board, and that the countdown value is consistent with `terminal_at` + the configured retention window.
3. Confirm a session terminated via the manual "Terminate"/"Remove" action (not an automatic merge) shows no badge, or "Removing shortly" briefly before the next GC tick removes the row — not a stale/incorrect countdown.

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-app/src/components/lifecycle_status.rs crates/ninox-app/src/components/sidebar.rs crates/ninox-app/src/components/fleet_board.rs crates/ninox-app/src/components/pr_list.rs
git commit -m "feat(app): show retention countdown badge and extend gate tooltip to PR list"
```

---

## Task 8: Final full-workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Full clean build**

Run: `cargo clean && cargo build --workspace`
Expected: builds with zero warnings introduced by this feature (pre-existing warnings, if any, are out of scope).

- [ ] **Step 2: Full test suite**

Run: `cargo test --workspace`
Expected: 100% pass, including every test added in Tasks 1–7.

- [ ] **Step 3: Targeted regression check on the exact bug PR #57 fixed**

Run: `cargo test -p ninox-core poll_github` and `cargo test -p ninox-core derive_session_status derive_status_preserves`
Expected: PASS — confirms the merge-on-apply change (Task 4) and the `PR_LINK` flagging (Task 3) didn't regress the specific `pr_id`-flicker scenario PR #57 fixed, nor the terminal-state-preservation guards `derive_session_status` relies on.

- [ ] **Step 4: Manual end-to-end pass**

Run the app (per Task 7 Step 8's run command) and repeat all three manual checks from Task 7 Step 8 once more against a clean build, plus: confirm the sidebar and fleet board never visibly flicker a session's PR card/status between two different values on an idle fleet (the original PR #57 symptom) — watch a session with an open PR for at least two full 30-second `poll_github` cycles without touching it.
