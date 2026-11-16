# `ninox user-request` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give orchestrators and workers a `ninox user-request` CLI that asks
the real human a question and gets back an answer that cannot be forged by a
peer agent session, closing the gap the sender-identity-attestation work
deliberately left open (a verified *agent* session is still not a human).

**Architecture:** A new `user_requests` store table (mirrors `work_requests`'
durability pattern) holds pending/answered/cancelled rows. The anti-spoof
property comes from two independent layers mirroring the existing
`orchestrator_auth.rs`/`OrchestratorRuntimeIdentity` precedent: (1) the
write function `Store::answer_user_request` has exactly two call sites in
the whole tree — `ninox-gui`'s `update()` and `ninox-tui`'s key handler —
enforced by a CI-checked call-site enumeration test, and (2) that function
independently verifies the *actual calling process* against a
self-registered `HumanConsoleIdentity`, gated at registration time by a
negative process-ancestry check against every known agent root pid. `sync`
(sign-off) answers are further restricted to `ninox-gui` only, in code, not
by a runtime flag.

**Tech Stack:** Rust workspace (`ninox-core`, `ninox-ui-shared`,
`ninox-gui` (iced), `ninox-tui` (ratatui)), SQLite via the existing `Store`,
`clap` for CLI.

**Spec:** `docs/superpowers/specs/2026-10-26-user-request-design.md`

## Global Constraints

- No `Command::UserRequestAnswer`/`Respond`/`Reply`/`SetAnswer` variant ever
  exists in `ninox-ui-shared`'s `Command` enum — no CLI/HTTP path may reach
  `answer_user_request`.
- `Store::answer_user_request` has exactly two call sites in the whole
  source tree: one in `ninox-gui`'s `app::update()`, one in `ninox-tui`'s
  key-event handler — enforced by a CI-checked enumeration test (Task 10).
- `sync`-mode requests are answerable only via `ninox-gui`; `ninox-tui` has
  no code path capable of writing a `sync` answer, structurally.
- `async`-mode answers may come from either `ninox-gui` or `ninox-tui`.
- `requester_session_id` is always resolved server-side via
  `worker_status::resolve_session_id`, never a CLI argument.
- Default `--timeout` is **240s**. A timeout leaves the row `pending` — it
  is the asking agent giving up, never a cancellation.
- `HumanConsoleIdentity` registration rejects any process whose ancestry
  includes a known orchestrator root pid or a live worker session's root
  pid (ancestry-gate). Documented residual risk: a detached
  (double-fork/`setsid`) process evades this — accepted, not fixed here.
- A `sync`-mode pending row has no dismiss affordance except the
  stale-session exception: dismissable only once `requester_session_id` is
  independently confirmed `Terminated`/`Interrupted`
  (`SessionStatus::is_terminal()`), and dismissal sets `status = cancelled`,
  never `answered`.
- Capability entry: `name: "user-request"`, `audience: Audience::Both`,
  `enabled: |_| true` — always on, no config toggle.
- No `list`/`cancel` CLI subcommands beyond `ask`/`wait`/`status` in v1.
- Remote (SSH-tunneled) sessions are out of scope for v1.

## Review Focus

- **A timed-out `ask` must leave the row `pending`, never `cancelled` or
  `answered`.** A careless implementation might mark a client-side timeout
  as abandonment. Pinned in Task 6.
- **A second `answer_user_request` call against an already-`answered` row
  must be a no-op**, not overwrite the existing `answer`/`answered_at`
  (e.g. a stale GUI panel double-submit). Pinned in Task 5.
- **`deliver_user_request_answers` must not crash the poll tick when the
  requester session has already terminated** before an `async` answer was
  delivered — log and continue, same posture as other per-target poller
  failures. Pinned in Task 7.
- **A pending row whose requester session has already terminated must
  still appear in the GUI/TUI listing** (badged "stale"), never silently
  disappear before the human can see and knowingly dismiss it. Pinned in
  Task 14.
- **Concurrent `wait` polling against a row being answered must observe a
  clean `pending`→`answered` transition**, never a torn read (e.g. `answer`
  set but `status` still `pending`). Pinned in Task 5's store test.

---

## Task 1: `user_requests` storage layer

**Files:**
- Modify: `crates/ninox-core/src/types.rs` — add `UserRequestMode`,
  `UserRequestStatus` enums near `SessionStatus` (types.rs:9-20 is the
  model).
- Modify: `crates/ninox-core/src/store_fleet.rs` — add table DDL,
  `UserRequestRow`, CRUD, near the existing `work_requests` section
  (schema at lines 28-36, struct at 112-124, methods at 305-363).

**Interfaces:**
- Produces:
  ```rust
  pub enum UserRequestMode { Sync, Async }
  pub enum UserRequestStatus { Pending, Answered, Cancelled }

  pub struct UserRequestRow {
      pub id: String,
      pub requester_session_id: String,
      pub prompt: String,
      pub mode: UserRequestMode,
      pub status: UserRequestStatus,
      pub answer: Option<String>,
      pub answered_at: Option<i64>,
      pub answer_delivered_at: Option<i64>,
      pub created_at: i64,
  }

  impl Store {
      pub fn insert_user_request(&self, r: &UserRequestRow) -> Result<()>;
      pub fn get_user_request(&self, id: &str) -> Result<Option<UserRequestRow>>;
      /// status == Pending, oldest first. `requester_session_id: None` = fleet-wide.
      pub fn open_user_requests(&self, requester_session_id: Option<&str>) -> Result<Vec<UserRequestRow>>;
      /// status == Answered && mode == Async && answer_delivered_at IS NULL, for one requester.
      pub fn undelivered_answered_user_requests(&self, requester_session_id: &str) -> Result<Vec<UserRequestRow>>;
      pub fn mark_user_request_answer_delivered(&self, id: &str, at: i64) -> Result<()>;
      /// Legal only from Pending; sets status = Cancelled. Never touches `answer`.
      pub fn mark_user_request_cancelled(&self, id: &str) -> Result<()>;
  }
  ```

Table DDL:
```sql
CREATE TABLE IF NOT EXISTS user_requests (
    id                    TEXT PRIMARY KEY,
    requester_session_id  TEXT NOT NULL,
    prompt                TEXT NOT NULL,
    mode                  TEXT NOT NULL,
    status                TEXT NOT NULL,
    answer                TEXT,
    answered_at           INTEGER,
    answer_delivered_at   INTEGER,
    created_at            INTEGER NOT NULL
);
```
`mode`/`status` are stored as their lowercase variant names (`"sync"`,
`"async"`, `"pending"`, `"answered"`, `"cancelled"`); give both enums
`FromStr`/`Display` (or a small manual `as_str`/`parse` pair) used by the
row-mapping code, the same way other TEXT-backed enums in this file are
mapped (follow the existing row-mapping idiom used for `WorkRequestRow` in
this file rather than introducing a new one).

- [ ] **Step 1: Write failing store tests**

```rust
#[test]
fn insert_and_get_user_request_round_trips() { /* insert a Sync/Pending row, get_user_request returns it unchanged */ }

#[test]
fn open_user_requests_filters_to_pending_and_requester() { /* one Pending + one Answered row for session A, one Pending for session B; open_user_requests(Some("A")) returns only A's pending row; open_user_requests(None) returns both pending rows */ }

#[test]
fn answering_twice_is_a_noop() {
    // insert pending row, mark it answered via a direct row update, then call
    // mark_user_request_answer_delivered twice and confirm the second call
    // doesn't touch answered_at — proves mark_* methods are idempotent guards,
    // the full double-answer guarantee is asserted end-to-end in Task 5.
}

#[test]
fn mark_user_request_cancelled_never_sets_answered_fields() { /* cancel a pending row; answer and answered_at remain None, status == Cancelled */ }
```

- [ ] **Step 2: Run tests, confirm they fail to compile (types/methods don't exist)**

Run: `cargo test -p ninox-core user_request`
Expected: compile error, `UserRequestRow`/`insert_user_request` not found.

- [ ] **Step 3: Implement the enums, struct, table DDL, and the five Store methods**

- [ ] **Step 4: Run tests, confirm they pass**

Run: `cargo test -p ninox-core user_request`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/types.rs crates/ninox-core/src/store_fleet.rs
git commit -m "feat(core): add user_requests storage layer"
```

---

## Task 2: `HumanConsoleIdentity` storage layer

**Files:**
- Modify: `crates/ninox-core/src/types.rs` — add `HumanConsoleIdentity`
  near `OrchestratorRuntimeIdentity` (types.rs:328-342 is the model).
- Modify: `crates/ninox-core/src/store.rs` — add CRUD near
  `register_orchestrator_runtime` (store.rs:537-566).

**Interfaces:**
- Produces:
  ```rust
  pub struct HumanConsoleIdentity {
      pub id: String,            // "hci-{root_pid}-{root_created_at}"
      pub root_pid: u32,
      pub root_created_at: i64,
      pub process_name: String,  // "ninox-gui" | "ninox-tui"
      pub registered_at: i64,
  }

  impl Store {
      pub fn register_human_console_identity(&self, identity: &HumanConsoleIdentity) -> Result<()>;
      pub fn list_human_console_identities(&self) -> Result<Vec<HumanConsoleIdentity>>;
      /// NEW — needed by the ancestry-gate check in Task 4; no such listing
      /// method exists today (only single-orchestrator lookups do).
      pub fn list_orchestrator_runtime_identities(&self) -> Result<Vec<OrchestratorRuntimeIdentity>>;
  }
  ```

Add a `human_console_identities` table with the same five columns as the
struct (`CREATE TABLE IF NOT EXISTS`, `id TEXT PRIMARY KEY`). Follow the
exact upsert style of `register_orchestrator_runtime` for
`register_human_console_identity` (`ON CONFLICT DO NOTHING` is wrong here
since each boot should refresh `registered_at`/re-prove ancestry — use
`ON CONFLICT(id) DO UPDATE SET registered_at = excluded.registered_at`).

- [ ] **Step 1: Write failing store tests**

```rust
#[test]
fn register_and_list_human_console_identity_round_trips() { /* register one, list_human_console_identities returns it */ }

#[test]
fn list_orchestrator_runtime_identities_returns_all_registered_rows() { /* register two distinct OrchestratorRuntimeIdentity rows via the existing register_orchestrator_runtime, assert both come back */ }
```

- [ ] **Step 2: Run tests, confirm failure (methods/type don't exist)**

Run: `cargo test -p ninox-core human_console_identity`

- [ ] **Step 3: Implement `HumanConsoleIdentity`, the table, and the three Store methods**

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p ninox-core human_console_identity`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/types.rs crates/ninox-core/src/store.rs
git commit -m "feat(core): add HumanConsoleIdentity storage layer"
```

---

## Task 3: `runtime::current_process_identity`

**Files:**
- Modify: `crates/ninox-core/src/tmux.rs` — widen `process_started_at`
  (tmux.rs:719-730) to `pub(crate)` if it isn't already.
- Modify: `crates/ninox-core/src/runtime/mod.rs` — add the public
  primitive, near `caller_descends_from` (runtime/mod.rs:684-686).

**Interfaces:**
- Consumes: `tmux::process_started_at(pid: u32) -> Result<i64>` (existing,
  `ps`-based).
- Produces:
  ```rust
  pub struct ProcessIdentity { pub pid: u32, pub created_at: i64 }
  pub fn current_process_identity() -> Result<ProcessIdentity>;
  ```
  Built from `std::process::id()` + `tmux::process_started_at(pid)` —
  unlike `TmuxPaneIdentity`, this does **not** require the calling process
  to be inside a tmux pane (the GUI never is), so it must not go through
  `current_private_pane_identity()`'s pane-listing path.

- [ ] **Step 1: Write a failing test**

```rust
#[test]
fn current_process_identity_matches_the_running_test_process() {
    let id = runtime::current_process_identity().unwrap();
    assert_eq!(id.pid, std::process::id());
    assert!(id.created_at > 0);
}
```

- [ ] **Step 2: Run test, confirm it fails to compile**

Run: `cargo test -p ninox-core current_process_identity`

- [ ] **Step 3: Implement `ProcessIdentity` and `current_process_identity()`**

- [ ] **Step 4: Run test, confirm it passes**

Run: `cargo test -p ninox-core current_process_identity`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/tmux.rs crates/ninox-core/src/runtime/mod.rs
git commit -m "feat(core): add runtime::current_process_identity"
```

---

## Task 4: `human_console_auth::register_human_console` (ancestry-gate)

**Files:**
- Create: `crates/ninox-core/src/human_console_auth.rs` (register `mod
  human_console_auth;` in `crates/ninox-core/src/lib.rs`).
- Test: in the same file's `mod tests`, modeled on
  `orchestrator_auth.rs`'s `mod tests` (orchestrator_auth.rs:274-546) —
  reuse its `open_store()` fixture style.

**Interfaces:**
- Consumes: `runtime::current_process_identity()` (Task 3),
  `runtime::caller_descends_from(root_pid: u32) -> bool`
  (`runtime/mod.rs:684-686`, existing), `Store::list_orchestrator_runtime_identities`
  + `Store::register_human_console_identity` (Task 2), `Store::list_sessions`
  (existing) for live worker pids via `Session.pid: Option<u32>`
  (`types.rs:92`).
- Produces:
  ```rust
  pub fn register_human_console(store: &Store, process_name: &str) -> Result<HumanConsoleIdentity>;
  ```
  Logic: collect known agent root pids = `{o.root_pid for o in
  list_orchestrator_runtime_identities()} ∪ {s.pid for s in list_sessions()
  if s.pid.is_some()}`; if `caller_descends_from(pid)` is true for *any* of
  them, return `Err` (refuse registration) without writing a row;
  otherwise build a `HumanConsoleIdentity` from
  `current_process_identity()` and `process_name`, call
  `register_human_console_identity`, return it.

- [ ] **Step 1: Write failing tests (includes adversarial test #6 from the spec)**

```rust
#[test]
fn registers_normally_with_no_known_agent_ancestry() {
    // open_store(); no orchestrators/sessions registered; register_human_console
    // succeeds and the returned identity matches current_process_identity().
}

#[test]
fn refuses_registration_when_an_ancestor_pid_is_a_known_orchestrator_root() {
    // register a fake OrchestratorRuntimeIdentity whose root_pid is the TEST
    // process's own pid (so caller_descends_from reports true), then assert
    // register_human_console returns Err.
}

#[test]
fn refuses_registration_when_an_ancestor_pid_is_a_live_worker_session_root() {
    // same shape, but via a Session row's `pid` field instead of an
    // OrchestratorRuntimeIdentity.
}

#[test]
fn agent_spawned_ninox_tui_cannot_register() {
    // spawn a child process (e.g. `std::process::Command::new(current_exe)`
    // with an env var telling it to call register_human_console and print
    // the result) from a parent that first registered itself as a fake
    // OrchestratorRuntimeIdentity rooted at the test process's own pid;
    // assert the child's registration attempt fails.
    // Confirm a sibling child spawned WITHOUT that fake-root ancestry
    // registers successfully — same process, no ancestor registered.
}
```

- [ ] **Step 2: Run tests, confirm they fail to compile**

Run: `cargo test -p ninox-core human_console_auth`

- [ ] **Step 3: Implement `register_human_console`**

The ancestry walk only needs `caller_descends_from`, already proven
correct by `orchestrator_auth.rs`'s own test suite — no new process-walking
code, just the known-roots aggregation described above.

- [ ] **Step 4: Run tests, confirm they pass**

Run: `cargo test -p ninox-core human_console_auth`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/human_console_auth.rs crates/ninox-core/src/lib.rs
git commit -m "feat(core): add HumanConsoleIdentity registration with ancestry gate"
```

---

## Task 5: `human_console_auth::verify_caller_is_registered_console` + `Store::answer_user_request`

**Files:**
- Modify: `crates/ninox-core/src/human_console_auth.rs` — add the
  verification function and its tests.
- Modify: `crates/ninox-core/src/store.rs` — add `answer_user_request`.

**Interfaces:**
- Consumes: `runtime::current_process_identity()` (Task 3),
  `Store::list_human_console_identities` (Task 2),
  `Store::get_user_request`/row update from Task 1.
- Produces:
  ```rust
  pub fn verify_caller_is_registered_console(store: &Store) -> Result<()>;
  impl Store {
      pub fn answer_user_request(&self, id: &str, answer: &str) -> Result<()>;
  }
  ```
  `verify_caller_is_registered_console` matches the calling process's
  `current_process_identity()` against every registered
  `HumanConsoleIdentity` on `root_pid` equality and `|root_created_at -
  created_at| <= 2_000` (milliseconds — same tolerance
  `runtime_identity_matches` uses, orchestrator_auth.rs:95-102); `Err` if no
  row matches. `answer_user_request` calls this first, then does a
  read-apply-write on the row: if `status != Pending`, returns `Ok(())`
  (idempotent no-op — Review Focus: double-answer must not overwrite);
  else sets `status = Answered`, `answer`, `answered_at`, writes it back.

- [ ] **Step 1: Write failing tests (includes adversarial test #4 from the spec)**

```rust
#[test]
fn verify_caller_fails_with_no_registered_identity() {
    // open_store(); no HumanConsoleIdentity registered; assert
    // verify_caller_is_registered_console returns Err.
}

#[test]
fn verify_caller_fails_against_a_different_fake_pid() {
    // register a HumanConsoleIdentity with a pid/created_at that is NOT the
    // test process's own; assert verify_caller_is_registered_console still
    // fails for the real calling process.
}

#[test]
fn verify_caller_succeeds_after_register_human_console() {
    // register_human_console(&store, "ninox-gui").unwrap(); assert
    // verify_caller_is_registered_console(&store) is Ok.
}

#[test]
fn answer_user_request_rejected_without_a_registered_console() {
    // insert a pending UserRequestRow; call store.answer_user_request
    // directly (ninox-core's own test suite, the most generous starting
    // position for a forger per the spec); assert Err and the row is still
    // Pending afterward.
}

#[test]
fn answering_an_already_answered_row_does_not_overwrite_it() {
    // register_human_console; answer a pending row with "first"; answer it
    // again with "second"; assert the stored answer is still "first" and
    // answered_at is unchanged — Review Focus #2.
}

#[test]
fn concurrent_wait_poll_never_observes_a_torn_answered_row() {
    // insert pending row; from a second thread, call answer_user_request;
    // on the polling thread, loop get_user_request until status != Pending
    // and assert that the SAME read that reports Answered also has
    // `answer.is_some()` — never Answered with answer == None.
}
```

- [ ] **Step 2: Run tests, confirm failure**

Run: `cargo test -p ninox-core answer_user_request`

- [ ] **Step 3: Implement `verify_caller_is_registered_console` and `Store::answer_user_request`**

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p ninox-core answer_user_request`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/human_console_auth.rs crates/ninox-core/src/store.rs
git commit -m "feat(core): add answer_user_request with process-identity verification"
```

---

## Task 6: CLI — `ninox user-request ask|wait|status`

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs` — new `Command::UserRequest`
  variant, hot-path dispatch, `run_user_request_{ask,wait,status}`. Model
  the hot-path short-circuit on `Whoami`/`Statusline` (cli.rs around
  lines 582-621), not on `RequestWork` (which does **not** short-circuit
  today, per CLAUDE.md this new command should).
- Test: a new `#[cfg(test)] mod user_request_cli_tests` in the same file
  (or an adjacent test module following this crate's existing convention).

**Interfaces:**
- Consumes: `worker_status::resolve_session_id(store, env_session, cwd) ->
  Result<Option<SessionId>>` (`worker_status.rs:214-228`, existing — same
  function `ninox send`'s `Command::Send` arm already uses, cli.rs:908-918),
  `Store::{insert_user_request, get_user_request}` (Task 1).
- Produces: a shared poll helper
  ```rust
  fn poll_until_resolved(store: &Store, id: &str, timeout: Duration) -> Result<PollOutcome>;
  enum PollOutcome { Answered(String), StillPending }
  ```
  used by both `ask` (non-`--async`) and `wait`.

```rust
#[derive(clap::Subcommand)]
pub enum UserRequestCommand {
    Ask {
        prompt: String,
        #[arg(long = "async")]
        async_mode: bool,
        #[arg(long, default_value_t = 240)]
        timeout: u64,
    },
    Wait {
        id: String,
        #[arg(long, default_value_t = 240)]
        timeout: u64,
    },
    Status {
        id: String,
    },
}
// on Command: UserRequest(UserRequestCommand),
```

Id format mirrors `hooks.rs`'s `wr-{ms}-{pid}-{seq}` scheme
(hooks.rs:463-470) exactly, but as `ur-{ms}-{pid}-{seq}` via a
function-local `static SEQ: AtomicU32` in `run_user_request_ask` — this
codebase duplicates that idiom per-call-site rather than sharing it
(confirmed: `hooks.rs`'s own `SEQ` is function-local too), so follow suit
rather than refactoring it out.

Poll loop interval: 1 second (spec's open question, resolved here as "pick
something cheap").

- [ ] **Step 1: Write failing CLI tests (includes adversarial test #2 and part of #1 from the spec)**

```rust
#[test]
fn user_request_ask_wait_status_parse() {
    // Command::try_parse_from(["ninox", "user-request", "ask", "proceed?"]) is Ok
    // Command::try_parse_from(["ninox", "user-request", "wait", "ur-1"]) is Ok
    // Command::try_parse_from(["ninox", "user-request", "status", "ur-1"]) is Ok
}

#[test]
fn user_request_has_no_answer_like_subcommand() {
    for bad in ["answer", "respond", "reply", "set-answer"] {
        let argv = ["ninox", "user-request", bad, "ur-1", "yes"];
        assert!(Command::try_parse_from(argv).is_err(), "{bad} must not parse");
    }
}

#[test]
fn ask_resolves_requester_session_server_side_not_from_argv() {
    // confirm run_user_request_ask's signature takes no --from/--session
    // argument at all — a compile-level assertion via the function
    // signature, not a runtime test (mirrors ninox send's posture).
}

#[test]
fn ask_timeout_leaves_the_row_pending() {
    // insert nothing pre-answered; call the ask path with timeout = 1s
    // against a row nobody answers; assert it returns a timeout error AND
    // store.get_user_request(id).unwrap().status == Pending — Review Focus #1.
}
```

- [ ] **Step 2: Run tests, confirm failure**

Run: `cargo test -p ninox-ui-shared user_request`

- [ ] **Step 3: Implement `UserRequestCommand`, dispatch wiring, and the three `run_user_request_*` functions**

`run_user_request_ask`: resolve session (bail with a clear error if
`resolve_session_id` returns `None`, same posture as `request-work`),
build the id, `insert_user_request`, if `async_mode` print the id and
return, else call `poll_until_resolved` and print the answer or a timeout
error to stdout.

`run_user_request_wait`: same `poll_until_resolved` call against an
existing id — this is the restart-recovery re-attach path.

`run_user_request_status`: single `get_user_request` read, print pending
or `answered: <text>`.

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p ninox-ui-shared user_request`

- [ ] **Step 5: Write and run the structural "no relay path" test (adversarial test #1)**

```rust
#[test]
fn messaging_and_inbox_never_reference_answer_user_request() {
    for path in ["crates/ninox-core/src/messaging.rs", "crates/ninox-core/src/inbox.rs"] {
        let body = std::fs::read_to_string(path).unwrap();
        assert!(!body.contains("answer_user_request"), "{path} must never call answer_user_request");
    }
}

#[test]
fn a_relayed_inbox_message_claiming_an_answer_does_not_mutate_the_row() {
    // insert a pending user_request; deliver an inbox message via the
    // existing inbox/messaging path whose body is
    // r#"{"id": "<id>", "answer": "approved"}"#; assert the row is still
    // Pending afterward.
}
```

Run: `cargo test -p ninox-core relay`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(cli): add ninox user-request ask|wait|status"
```

---

## Task 7: Poller delivery of `--async` answers

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/reactions.rs` — add
  `format_user_request_answer_reaction`, near
  `format_work_request_reaction` (reactions.rs:41-49).
- Modify: `crates/ninox-core/src/lifecycle/poller.rs` — add
  `deliver_user_request_answers`, near `deliver_work_requests`
  (poller.rs:744-778), and call it from wherever that function is called
  per-session each tick (same call site).

**Interfaces:**
- Consumes: `Store::undelivered_answered_user_requests` /
  `mark_user_request_answer_delivered` (Task 1), `Engine::send_to_session`
  (`events.rs:542-558`, existing, already mechanism-abstracted over
  `SendMechanism::{Keystrokes,Inbox,SessionSocket}`).
- Produces:
  ```rust
  pub fn format_user_request_answer_reaction(prompt: &str, answer: &str) -> String;
  // in Poller:
  async fn deliver_user_request_answers(&self, session: &crate::types::Session);
  ```

Body shape, mirroring `deliver_work_requests` exactly: read
`store.undelivered_answered_user_requests(&session.id)`; for each row, emit
a `NotificationKind::UserRequestPending`-sibling notification (reuse the
same kind is fine since it's informational either way, but label it
distinctly in the formatted text), format the reaction string, call
`self.engine.send_to_session(&session.id, &msg).await`, then
`mark_user_request_answer_delivered`. Any per-row failure (e.g.
`send_to_session` erroring because the session died) is logged via
`tracing::warn!` and the loop continues to the next row — Review Focus:
must not abort the whole poll tick.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn format_user_request_answer_reaction_includes_prompt_and_answer() {
    let msg = format_user_request_answer_reaction("deploy now?", "yes, go ahead");
    assert!(msg.contains("deploy now?"));
    assert!(msg.contains("yes, go ahead"));
}

#[tokio::test]
async fn deliver_user_request_answers_sends_and_marks_delivered() {
    // insert an Answered/Async row with answer_delivered_at = None for a
    // live session; run deliver_user_request_answers; assert
    // mark_user_request_answer_delivered was effectively called (row now has
    // answer_delivered_at set) and (via a fake send_to_session / engine
    // test double, following this module's existing poller test fixtures)
    // that a message was sent to that session.
}

#[tokio::test]
async fn deliver_user_request_answers_does_not_panic_for_a_dead_requester() {
    // same setup but the session row is Terminated; assert the function
    // returns normally (no panic) and logs rather than propagating an error
    // that would abort the caller's poll tick — Review Focus #3.
}
```

- [ ] **Step 2: Run tests, confirm failure**

Run: `cargo test -p ninox-core deliver_user_request_answers`

- [ ] **Step 3: Implement `format_user_request_answer_reaction` and `deliver_user_request_answers`, and wire the call into the existing per-session poll tick alongside `deliver_work_requests`**

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p ninox-core deliver_user_request_answers`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/lifecycle/reactions.rs crates/ninox-core/src/lifecycle/poller.rs
git commit -m "feat(core): deliver async user-request answers via the poller"
```

---

## Task 8: Fleet snapshot, briefing, and fleet-recovery skill line (restart survival)

**Files:**
- Modify: `crates/ninox-core/src/fleet/snapshot.rs` — add
  `open_user_requests` field + `user_requests_for` accessor, mirroring
  `requests_for` (snapshot.rs:168).
- Modify: `crates/ninox-core/src/fleet/briefing.rs` — add a
  user-requests section to `orchestrator_briefing` (briefing.rs:123,
  153-164 is the model); check whether an analogous worker-facing briefing
  function exists in this file/module and add the same section there too
  if so, since `user_requests` applies to both audiences (unlike
  `work_requests`, which is orchestrator-only).
- Modify: `crates/ninox-core/skills/orchestrator/fleet-recovery.md` and
  `crates/ninox-core/skills/worker/fleet-recovery.md` — add one line.

**Interfaces:**
- Consumes: `Store::open_user_requests(None)` (Task 1).
- Produces:
  ```rust
  // FleetSnapshot gains:
  open_user_requests: Vec<UserRequestRow>,
  pub fn user_requests_for<'a>(&'a self, session_id: &'a str) -> impl Iterator<Item = &'a UserRequestRow> + 'a;
  ```
  Filter on `requester_session_id == session_id` (unlike `requests_for`,
  which filters on `orchestrator_id` — `user_requests` are owned by
  whichever session asked, worker or orchestrator).

Briefing text per row: if `answer.is_some()`, include the answer directly
("no re-wait needed" per spec); else instruct `ninox user-request wait
<id> --timeout <n>` to re-attach.

fleet-recovery.md line (both copies): *"If your briefing lists a pending
user-request, call `ninox user-request wait <id>` to re-attach rather than
asking again."* — placed right after the existing `request-work` bullet in
the orchestrator copy (fleet-recovery.md line 19 area).

- [ ] **Step 1: Write a failing test**

```rust
#[test]
fn orchestrator_briefing_lists_unresolved_user_requests_and_answers() {
    // FleetSnapshot with one unanswered and one answered user_requests row
    // for the same session; assert the rendered briefing contains the
    // unanswered row's "ninox user-request wait <id>" instruction and the
    // answered row's answer text directly.
}
```

- [ ] **Step 2: Run test, confirm failure**

Run: `cargo test -p ninox-core briefing_lists_unresolved_user_requests`

- [ ] **Step 3: Implement the snapshot field/accessor and the briefing section(s)**

- [ ] **Step 4: Run test, confirm pass**

Run: `cargo test -p ninox-core briefing_lists_unresolved_user_requests`

- [ ] **Step 5: Edit both `fleet-recovery.md` copies, adding the one new line**

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/fleet/snapshot.rs crates/ninox-core/src/fleet/briefing.rs crates/ninox-core/skills/orchestrator/fleet-recovery.md crates/ninox-core/skills/worker/fleet-recovery.md
git commit -m "feat(core): surface pending user-requests in fleet-recovery briefing"
```

---

## Task 9: Capability registry entry + skill docs

**Files:**
- Create: `crates/ninox-core/skills/orchestrator/user-request.md`
- Create: `crates/ninox-core/skills/worker/user-request.md`
- Modify: `crates/ninox-core/src/capabilities.rs` — add the `REGISTRY`
  entry (model: the `brain` entry, capabilities.rs:228-236, since this is
  a symmetric `Audience::Both` ungated entry, not the split `watch-pr`
  shape) and update `registry_covers_every_migrated_skill`'s hardcoded name
  lists (capabilities.rs:441-471) to include `"user-request"` in both the
  orchestrator and worker vectors.

**Interfaces:**
- Produces:
  ```rust
  Capability {
      name: "user-request",
      audience: Audience::Both,
      orchestrator_md: Some(include_str!("../skills/orchestrator/user-request.md")),
      worker_md: Some(include_str!("../skills/worker/user-request.md")),
      enabled: |_| true,
      hook: None,
  },
  ```

Both markdown files use the `watch-pr.md` frontmatter/body style (YAML
frontmatter `name:`/`description:`, terse imperative `##`-sectioned body,
fenced `bash` command examples). The **worker** body must say explicitly,
per the spec's Implementation pattern section, that only a real GUI/TUI
interaction can answer — so agents don't waste time trying to relay
answers to each other — and must describe `ask [--async] [--timeout]`,
`wait`, `status`, the 240s default timeout, "a timeout is normal, call
`wait` again rather than re-asking," and the spec's headless-case open
question: if nobody is running `ninox-gui`/`ninox-tui`, a `sync` request
simply blocks until one is launched — that's expected, not a sign the
feature is broken. The **orchestrator** body covers the same CLI surface
plus a line cross-referencing the fleet-recovery skill's new line from
Task 8.

- [ ] **Step 1: Write failing registry tests**

```rust
#[test]
fn user_request_capability_is_registered_for_both_audiences() {
    let names: Vec<_> = REGISTRY.iter().filter(|c| c.name == "user-request").collect();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].audience, Audience::Both);
    assert!((names[0].enabled)(&AppConfig::default()));
}
```

(The existing invariant tests —
`every_entry_has_markdown_for_its_audience`,
`every_markdown_has_frontmatter_with_a_parseable_description`,
`rendering_every_orchestrator_md_leaves_no_placeholders`,
`worker_md_contains_no_placeholders` — automatically cover the new entry
once it's added; no new test needed for those.)

- [ ] **Step 2: Run test, confirm failure**

Run: `cargo test -p ninox-core user_request_capability`

- [ ] **Step 3: Write the two skill markdown files and add the REGISTRY entry; update `registry_covers_every_migrated_skill`'s name lists**

- [ ] **Step 4: Run the full capabilities test module, confirm everything passes**

Run: `cargo test -p ninox-core capabilities::`
Expected: PASS, including the pre-existing invariant/seeding tests now
covering `user-request`.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/skills/orchestrator/user-request.md crates/ninox-core/skills/worker/user-request.md crates/ninox-core/src/capabilities.rs
git commit -m "feat(core): register user-request as an agent-facing capability"
```

---

## Task 10: Call-site enumeration CI check (adversarial test #3)

**Files:**
- Create: a small test (new file `crates/ninox-core/tests/
  answer_user_request_call_sites.rs`, a workspace-level integration test so
  it can grep across crate boundaries) or, if this workspace already has a
  precedent location for cross-crate structural tests, follow that
  instead — check for one (e.g. a `tests/` directory at the workspace
  root, or an existing enumeration-style test near
  `registry_covers_every_migrated_skill`) before creating a new location.

**Interfaces:**
- Consumes: nothing from earlier tasks except the existence of
  `answer_user_request` itself (Task 5) and its two intended call sites
  (Tasks 11, 12).
- Produces: a test that fails the build the moment a third call site
  appears anywhere in the workspace.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn answer_user_request_has_exactly_its_two_intended_call_sites() {
    let workspace_root = /* CARGO_MANIFEST_DIR's workspace ancestor */;
    let mut call_sites = Vec::new();
    for entry in walk_rust_files(&workspace_root) {
        let body = std::fs::read_to_string(&entry).unwrap();
        for (line_no, line) in body.lines().enumerate() {
            if line.contains("answer_user_request") {
                call_sites.push((entry.clone(), line_no));
            }
        }
    }
    // Expected matches: the fn definition + doc comment in store.rs, this
    // test file itself, Task 5's unit tests in human_console_auth.rs/store.rs,
    // and exactly one line each in ninox-gui's app.rs and ninox-tui's
    // state.rs (or wherever Task 11/12 land their call sites).
    let non_test_non_defn_call_sites: Vec<_> = call_sites.iter()
        .filter(|(p, _)| !p.ends_with("store.rs") && !p.to_string_lossy().contains("test"))
        .collect();
    assert_eq!(non_test_non_defn_call_sites.len(), 2,
        "expected exactly 2 call sites (ninox-gui, ninox-tui), found: {non_test_non_defn_call_sites:?}");
}
```

Write this test *before* Tasks 11/12 land their call sites (this task runs
after Task 9 in file order, but functionally depends on the final file
paths from Tasks 11/12 — note in the step: the exact filter logic may need
a one-line adjustment once those land; that's expected and is itself part
of proving the test actually enforces the count, not just passes
vacuously).

- [ ] **Step 2: Run the test now (before Tasks 11/12), confirm it correctly reports 0 call sites**

Run: `cargo test --workspace answer_user_request_has_exactly`
Expected: FAIL with "found: 0 call sites" (proves the test's detection
logic actually works, rather than trivially passing).

- [ ] **Step 3: Commit this test as-is; it will naturally start passing once Tasks 11 and 12 land their single call sites each**

```bash
git add crates/ninox-core/tests/answer_user_request_call_sites.rs
git commit -m "test: enforce exactly two call sites for answer_user_request"
```

- [ ] **Step 4: After Tasks 11 and 12 are both complete, re-run this test and confirm it passes**

Run: `cargo test --workspace answer_user_request_has_exactly`
Expected: PASS, exactly 2 non-definition, non-test call sites.

---

## Task 11: GUI — Pending Requests panel

**Files:**
- Modify: `crates/ninox-core/src/types.rs` — add
  `NotificationKind::UserRequestPending` (types.rs:455-491).
- Modify: `crates/ninox-gui/src/components/notification_panel.rs` —
  extend `kind_label()` (16-34) and `kind_color()` (36-54) matches.
- Create: `crates/ninox-gui/src/components/pending_requests_panel.rs`
  (model: `notification_panel.rs` itself).
- Modify: `crates/ninox-gui/src/components/sidebar.rs` — push the new
  panel unconditionally into `col_items` (259-269 is the model; unlike the
  notification panel, this is **not** gated by a show/hide toggle — it is
  always rendered per the spec's "persistent panel" requirement).
- Modify: `crates/ninox-gui/src/app.rs` — add `Message::AnswerUserRequest
  { id: String, answer: String }` (near 439-441), its `update()` arm (model:
  the `Message::SpawnFormConfirm` arm's direct store-call idiom, app.rs
  ~1676), a `state.pending_user_requests: Vec<UserRequestRow>` field
  refreshed alongside wherever `state.sessions`/`state.notifications`
  already get repopulated on the app's periodic refresh tick.
- Modify: `ninox-gui`'s application entry point (its `main.rs`/`App::new`
  equivalent, before entering the iced run loop) — call
  `human_console_auth::register_human_console(&store, "ninox-gui")` once
  at startup; log and continue (don't crash the app) if it errors, since a
  detached/weird launch should degrade to "can't answer sync requests"
  rather than refusing to start.

**Interfaces:**
- Consumes: `Store::answer_user_request` (Task 5),
  `Store::open_user_requests(None)` (Task 1),
  `human_console_auth::register_human_console` (Task 4).
- Produces: this file's `pending_requests_panel(app: &App) -> Element<Message>`
  is the **only** call site of `answer_user_request` in `ninox-gui`
  (enforced by Task 10).

Panel behavior per the spec's Surfacing section: every pending request is
listed, labeled by requester session; `sync` rows get a visually stronger
treatment (no dismiss button at all, except the stale-session exception —
deferred to Task 14) and Approve/Deny quick-action buttons plus a freeform
text input, both firing `Message::AnswerUserRequest`; clicking into an
entry reveals the input (no global focus steal). `async` rows get the same
input, no visual escalation.

- [ ] **Step 1: Write a failing GUI test**

```rust
#[test]
fn answer_user_request_message_calls_store_and_clears_pending_entry() {
    // build a minimal App/state fixture (this crate's existing test
    // harness style — check for one near the SpawnFormConfirm tests) with
    // one pending UserRequestRow in state.pending_user_requests; dispatch
    // Message::AnswerUserRequest { id, answer: "approved".into() }; assert
    // store.get_user_request(&id) is now Answered with answer == "approved",
    // and state.pending_user_requests no longer contains that id.
}
```

- [ ] **Step 2: Run test, confirm failure**

Run: `cargo test -p ninox-gui answer_user_request`

- [ ] **Step 3: Implement the `NotificationKind` variant, the panel component, the `Message` arm, the sidebar wiring, the periodic refresh of `state.pending_user_requests`, and the startup `register_human_console` call**

- [ ] **Step 4: Run test, confirm pass**

Run: `cargo test -p ninox-gui answer_user_request`

- [ ] **Step 5: Start the GUI and manually verify**: a pending request
  (insert one via `ninox user-request ask "test?" --async` in a second
  terminal) appears in the persistent panel without stealing focus, and
  submitting an answer there is reflected by `ninox user-request status
  <id>`.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/types.rs crates/ninox-gui/src/components/notification_panel.rs crates/ninox-gui/src/components/pending_requests_panel.rs crates/ninox-gui/src/components/sidebar.rs crates/ninox-gui/src/app.rs
git commit -m "feat(gui): add persistent Pending Requests panel"
```

---

## Task 12: TUI — pending-requests screen

**Files:**
- Modify: `crates/ninox-tui/src/tui/state.rs` — add `View::UserRequests`
  (126 is the model), `TuiState` fields `pending_user_requests:
  Vec<UserRequestRow>`, `in_tmux: bool`, bind `u` in `command()` (near
  `Char('p')` at 1388) to `switch_view(View::UserRequests)`, add the
  `View::UserRequests => return user_requests_key(st, key),` arm to
  `handle_key`'s dispatch (1211-1216 is the model).
- Create: `crates/ninox-tui/src/tui/user_requests.rs` — `fn
  user_requests_key(st: &mut TuiState, key: KeyEvent) -> Action` (model:
  `pr_key`, state.rs:1754) and its list-filtering helpers (model:
  `prs.rs`).
- Modify: `crates/ninox-tui/src/tui/view.rs` — add `fn
  draw_user_requests(f: &mut Frame, lay: &Layout, st: &TuiState)` (model:
  `draw_prs`, view.rs:1010); extend `draw_footer` (272) to always show a
  "N pending user requests — press u to view" segment when
  `pending_user_requests` is non-empty, independent of `st.notice`.
- Modify: `crates/ninox-tui/src/tui/mod.rs` — at startup (`run`, line 45)
  set `st.in_tmux = std::env::var("TMUX").is_ok()` and call
  `human_console_auth::register_human_console(&store, "ninox-tui")`
  (log-and-continue on error, same posture as Task 11's GUI call); in
  `refresh()` (1233) add `st.pending_user_requests =
  store.open_user_requests(None).unwrap_or_default();` next to the
  existing `st.pr_watches` line; add an `Action` variant (model: whatever
  `open_pr`/`pr_key` already use to signal a store-mutating side effect
  back to the caller) for `AnswerUserRequest { id, answer }` and
  `CancelUserRequest { id }`, handled in the same action-dispatch match
  that currently handles `pr_key`'s actions — this is the **only** call
  site of `answer_user_request` in `ninox-tui`.

**Interfaces:**
- Consumes: `Store::answer_user_request` (Task 5, **only** for `mode ==
  Async` rows — the handler must have no code path at all that calls it
  for a `Sync` row), `Store::open_user_requests` (Task 1),
  `human_console_auth::register_human_console` (Task 4).

Per the spec's Surfacing section: selecting a `sync` row shows a read-only
"use ninox-gui to answer" note (`st.notify(Level::Info, "...")`) — no input
capture, ever. Selecting an `async` row and pressing Enter opens an inline
answer-input sub-mode (capture subsequent key input into a draft string
field on `TuiState` until Enter submits, returning
`Action::AnswerUserRequest`). The screen always renders the tmux caveat
line from the spec ("answers here are exposed to keystroke-injection...")
when `st.in_tmux` is true.

- [ ] **Step 1: Write a failing test**

```rust
#[test]
fn sync_row_has_no_answer_key_path() {
    // build a TuiState with one pending Sync UserRequestRow selected in the
    // user-requests screen; press Enter; assert the returned Action is
    // NOT AnswerUserRequest (it's Action::None or a Notify-style action) —
    // this is the structural proof that ninox-tui's own code has no call
    // path from its key handler to a sync-answer write.
}

#[test]
fn async_row_enter_then_submit_returns_answer_action() {
    // same but with an Async row: Enter opens the draft input, typing
    // "yes" then Enter returns Action::AnswerUserRequest { id, answer: "yes" }.
}
```

- [ ] **Step 2: Run tests, confirm failure**

Run: `cargo test -p ninox-tui user_requests`

- [ ] **Step 3: Implement the screen: state fields, key handler, renderer, footer badge, startup/refresh wiring**

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p ninox-tui user_requests`

- [ ] **Step 5: Run the TUI (`nx`) manually and verify**: a pending
  request shows in the footer badge, pressing `u` opens the screen, a
  `sync` row cannot be answered there, an `async` row can.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-tui/src/tui/state.rs crates/ninox-tui/src/tui/user_requests.rs crates/ninox-tui/src/tui/view.rs crates/ninox-tui/src/tui/mod.rs
git commit -m "feat(tui): add pending-requests screen with async-only answering"
```

---

## Task 13: Adversarial test — tmux pane injection into `ninox-tui`

**Files:**
- Test: `crates/ninox-tui/tests/tmux_injection.rs` (new integration test;
  requires a `tmux` binary available in CI — follow this crate's existing
  convention for gating tests that need external tools, if one exists,
  e.g. `#[ignore]` with a documented `cargo test -- --ignored` CI step, or
  a `which::which("tmux").is_ok()` runtime skip).

**Interfaces:**
- Consumes: the full TUI stack from Task 12, `tmux send-keys`
  (shelled out, not a crate dependency — matches this codebase's existing
  `ps`-based process-introspection style of shelling out rather than
  adding a dependency).

- [ ] **Step 1: Write the failing adversarial test (spec's test #5)**

```rust
#[test]
fn tmux_pane_injection_cannot_answer_a_sync_request() {
    // launch a real `ninox-tui`/`nx` process inside a fresh tmux session
    // (tmux new-session -d), pointed at a test store with one pending Sync
    // UserRequestRow addressed to some session; press 'u' then navigate to
    // and "Enter" the row via `tmux send-keys`; assert the row is still
    // Pending afterward — not because the keystrokes were rejected, but
    // because the sync-row key path has no answer call at all (Task 12
    // already proves this structurally; this test proves it end-to-end
    // through a real pty).
}

#[test]
fn tmux_pane_injection_can_answer_an_async_request_the_documented_weaker_tier() {
    // same setup with an Async row; drive the same navigate + Enter +
    // type-answer + Enter sequence via tmux send-keys; assert the row IS
    // Answered afterward — proving the weaker tier behaves exactly as
    // specified, neither stronger nor weaker than documented.
}
```

- [ ] **Step 2: Run tests, confirm the harness itself works (both tests should already pass once Task 12 is merged, since this is confirmatory, not driving new implementation) — if either fails, that's a real regression in Task 12's sync/async separation, not a new feature to build**

Run: `cargo test -p ninox-tui --test tmux_injection -- --ignored` (or
without `--ignored` depending on the gating chosen in Step 1)

- [ ] **Step 3: Commit**

```bash
git add crates/ninox-tui/tests/tmux_injection.rs
git commit -m "test: adversarial tmux-injection coverage for sync vs async user-requests"
```

---

## Task 14: Stale-request dismissal

**Files:**
- Modify: `crates/ninox-core/src/store_fleet.rs` — `
  mark_user_request_cancelled` already exists from Task 1; no new Store
  method needed here, just the staleness predicate used by both UIs.
- Modify: `crates/ninox-gui/src/components/pending_requests_panel.rs` —
  add the stale badge + dismiss button, gated on the requester session's
  `SessionStatus::is_terminal()` (`types.rs:32`), visible via
  `state.sessions`.
- Modify: `crates/ninox-tui/src/tui/user_requests.rs` — add the same
  stale badge + a dismiss keybinding (e.g. `x`) returning
  `Action::CancelUserRequest { id }`, gated the same way via
  `st.rows`/session lookup.

**Interfaces:**
- Consumes: `Store::mark_user_request_cancelled` (Task 1),
  `SessionStatus::is_terminal()` (existing).
- Produces: `Message::CancelUserRequest(id: String)` in `ninox-gui`
  (new arm in `app.rs`'s `update()`, alongside Task 11's
  `Message::AnswerUserRequest`, calling
  `store.mark_user_request_cancelled(&id)` then removing the row from
  `state.pending_user_requests`) and reuses `ninox-tui`'s
  `Action::CancelUserRequest { id }` already added in Task 12.

A pending row is dismissable **only** once its `requester_session_id`'s
live session status (looked up fresh, not cached at request-creation time)
is `is_terminal()` — never on human say-so alone while the session still
looks alive. Both the GUI panel and the TUI screen must still **list** a
stale row (badged, not hidden) — Review Focus: a pending row whose
requester died must remain visible until explicitly dismissed, never
silently vanish.

- [ ] **Step 1: Write failing tests (one per UI)**

```rust
// ninox-gui
#[test]
fn stale_row_is_shown_with_a_dismiss_badge_not_hidden() {
    // one pending UserRequestRow whose requester session is Terminated;
    // render the panel; assert the row still appears (not filtered out)
    // and carries the "stale" badge + a dismiss affordance.
}

#[test]
fn dismissing_a_stale_row_cancels_never_answers() {
    // dismiss it; assert status == Cancelled, answer still None.
}

#[test]
fn a_pending_row_for_a_live_session_has_no_dismiss_affordance() {
    // same shape but the requester session is not terminal; assert no
    // dismiss control is offered.
}
```

```rust
// ninox-tui — mirror the three cases above via user_requests_key /
// draw_user_requests instead of the GUI panel.
```

- [ ] **Step 2: Run tests, confirm failure**

Run: `cargo test -p ninox-gui stale_row` and `cargo test -p ninox-tui stale_row`

- [ ] **Step 3: Implement the staleness predicate and dismiss affordance in both UIs**

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p ninox-gui stale_row` and `cargo test -p ninox-tui stale_row`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-gui/src/components/pending_requests_panel.rs crates/ninox-tui/src/tui/user_requests.rs
git commit -m "feat: allow dismissing user-requests only once the asking session is terminal"
```

---

## Final verification

- [ ] `cargo build --workspace`
- [ ] `cargo test --workspace` (confirms Task 10's call-site enumeration
  test now passes with exactly 2 call sites, and every adversarial test
  from the spec's Testing section — #1 through #6 — is green)
- [ ] `cargo clippy --workspace --all-targets`
- [ ] Manual smoke test: run an orchestrator session, call `ninox
  user-request ask "ship it?"` from a worker, confirm it blocks; answer it
  from `ninox-gui`'s Pending Requests panel; confirm the worker's `ask`
  call returns the answer and exits 0.
