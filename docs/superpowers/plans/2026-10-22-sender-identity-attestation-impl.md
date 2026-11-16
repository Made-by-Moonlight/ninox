# Daemon-Attested Sender Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> This task runs as a solo autonomous ninox worker with no human reviewer available mid-implementation. Execution proceeds **native** (self-implemented task-by-task in this session), per the task brief's own Step 3-4 instructions.

**Goal:** Every message delivered by `ninox_core::messaging::deliver_message` (inbox, SessionSocket, Keystrokes) carries a daemon-verified sender-relationship line baked into the delivered content, generalized from `orchestrator_auth.rs`'s existing anti-spoofing checks.

**Architecture:** Extract a shared `attest_sender()` function (plus small helpers) into `orchestrator_auth.rs`, reusing its persisted-identity/process-ancestry primitives for orchestrator senders and a new live-pane-name check for worker senders. `messaging.rs` calls it once per delivery, renders a plain-text verification line, and prepends it to the message content before any mechanism sees it. Callers that resolve a sender from a live CLI invocation (`ninox send`, the orchestrator initial-brief send) capture their own process's pane identity and thread it through as proof; callers that relay ninox's own trusted content (`SYSTEM_SENDER`) skip attestation entirely.

**Tech Stack:** Rust, existing `ninox-core`/`ninox-ui-shared` crates, `tokio` async, `rusqlite`-backed `Store`.

**Spec:** `docs/superpowers/specs/2026-10-21-sender-identity-attestation-design.md`

## Citation corrections (verified against current `v1`, commit `aab6322`)

- `orchestrator_auth.rs` is 74 lines (not 66 — grew slightly from `v1-nested-orchestration`/PR #200's `fix(spawn): verify caller owns the claimed orchestrator id`, which is already merged into current `v1` and already reflected in the code read for this plan).
- `OrchestratorRuntimeIdentity` in `types.rs` is at lines **321-335** (doc comment + struct), not 320-332 nor the spec footnote's corrected 326-335 (drifted further since the footnote was written — the struct body itself is 326-335, matching the footnote, but citing "the struct" should include its doc comment starting at 321).
- `ninox send`'s sender-resolution code is **not in `main.rs` at all** — PR #194 (`refactor: split ninox-app into ninox-gui and ninox-tui`) moved all CLI dispatch logic into `crates/ninox-ui-shared/src/cli.rs`. The `Command::Send` arm (sender resolution + `deliver_message` call) is now at `cli.rs:864-886`, with the `resolve_session_id` call at `cli.rs:877-881`. This is a further drift beyond what the spec's footnote corrected; call it out in the PR body.
- `deliver_message`/`deliver_by_mechanism` are at `messaging.rs:80-123` (matches spec's description; the file has grown to 496 lines total with its test module, but the two functions' location is accurate).

## Open-question resolutions (spec's three open questions)

1. **Attestation strength for `SessionSocket`/`Keystrokes` paths:** Resolved as a non-issue once correctly scoped — attestation is a property of **who claims to be sending** (resolved once, synchronously, in the same process that invoked `ninox send`), not of which mechanism later carries the message to the receiver. `deliver_message` computes the attestation once and prepends the same rendered line regardless of `mechanism`. The real asymmetry is **orchestrator vs. worker senders**, not mechanism: orchestrators get the full strength `orchestrator_auth.rs` already uses (persisted `OrchestratorRuntimeIdentity` + process-ancestry + tmux-pane + server-epoch cross-check, self-registering on first use exactly as `authorize_orchestrator` does). Workers have no equivalent persisted immutable record, so their attestation instead checks that the calling process's own live pane is literally named after the claimed worker session id (`PaneIdentity::physical_tmux_name == claimed_sender`) — both backends (`tmux`, `ptyd`) only ever resolve `current_private_pane_identity()`/`current_pane_identity()` to `Some` after already confirming the calling process descends from that pane's live root process, so this still carries a genuine (if weaker — no persisted baseline across pane respawns) process-ancestry guarantee. The delivered message's trailing clause differs honestly between the two: `"confirmed via process-ancestry + tmux-pane cross-check"` for orchestrators vs. `"confirmed via live session-pane identity check"` for workers.
2. **`descendant` vs. `unrelated` distinction:** Kept textually and structurally distinct. Collapsing them would erase exactly the information this spec exists to add — whether an "unverified peer" is actually the agent's own spawned worker reporting back (`descendant`) versus a session with no recorded relationship at all (`unrelated`).
3. **Cross-machine attestation:** Out of scope, unchanged. `attest_sender` only ever compares same-machine, same-daemon state (`Store`, live pane identities); nothing in this plan touches remote/SSH sessions.

## Global Constraints

- Every relationship category from the spec is implemented exactly: `direct_orchestrator`, `sibling`, `descendant`, `unrelated`, `unverifiable`.
- `unverifiable` is reported plainly and never silently dropped or upgraded to a default relationship.
- The verification line is part of the delivered message **content** for all three mechanisms (inbox, SessionSocket, Keystrokes) — never a side channel.
- `SYSTEM_SENDER` (`"ninox"`) messages are never attested or annotated — they are ninox's own trusted, internally-generated content, not inter-session relays (existing doc comment in `messaging.rs:10-20` already establishes this distinction).
- No change to the Claude Code harness's own "unverified peer" safety framing.
- No cross-machine attestation.
- `cargo build`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets` must all be clean before opening the PR.
- Do not run `cargo fmt` on the whole tree.

## Review Focus

- **Tampered/forged sender claim** (a caller passes a `from` that doesn't match its own live pane): must render `verified: false` with relationship `unrelated`, never `direct_orchestrator`/`sibling`/`descendant` — this is the forgery case the whole spec exists to close.
- **Attestation attempted with no pane identity available at all** (e.g., `current_private_pane_identity()` returns `None` — caller isn't in any tracked pane, or ancestry couldn't be confirmed): must render `unverifiable`, not silently fall through to `unrelated` or skip the line entirely.
- **`from: None`** (today's existing fallback when `resolve_session_id` can't place the caller) must still get a verification line (`unverifiable`), not an unannotated message — this is the exact gap in today's behavior (a `ninox send` from an unresolvable caller currently gets delivered with no sender framing at all).
- **Orchestrator sender with no persisted `OrchestratorRuntimeIdentity` yet** (never called an orchestrator-facing command before its first `ninox send`): must still self-register via the same migration path `authorize_orchestrator` uses, so a legitimate first-use orchestrator doesn't get spuriously reported as unverified.
- **Each of the three delivery mechanisms independently** carries the attestation line in its delivered content — a regression that only annotates, say, the inbox path (today's existing gap) must be caught.

---

## Task 1: Extract and generalize attestation primitives in `orchestrator_auth.rs`

**Files:**
- Modify: `crates/ninox-core/src/orchestrator_auth.rs`

**Interfaces:**
- Consumes: `crate::runtime::PaneIdentity`, `crate::runtime::caller_descends_from`, `crate::store::Store::{is_orchestrator, get_session, orchestrator_runtime_identity, register_migrated_orchestrator_runtime}`, `crate::types::{OrchestratorRuntimeIdentity, Session}`.
- Produces (for Task 2 and tests):
  - `pub enum SenderRelationship { DirectOrchestrator, Sibling, Descendant, Unrelated, Unverifiable }` (derive `Debug, Clone, Copy, PartialEq, Eq`).
  - `pub struct SenderAttestation { pub claimed_session_id: Option<String>, pub relationship: SenderRelationship, pub verified: bool }` (derive `Debug, Clone, PartialEq, Eq`) with a method `pub fn render(&self) -> String`.
  - `pub fn attest_sender(store: &Store, claimed_sender: Option<&str>, caller_pane: Option<&runtime::PaneIdentity>, receiver_session_id: &str) -> SenderAttestation`.

- [ ] **Step 1: Write failing unit tests for `attest_sender` covering every relationship category**

Add a `#[cfg(test)] mod tests` block in `orchestrator_auth.rs` (new — the file has none today). Build a `Store` fixture with: an orchestrator `orch-1` with a persisted `OrchestratorRuntimeIdentity` (reuse the exact field shape from `types.rs:326-335`; pick arbitrary but internally-consistent `server_epoch`/`physical_tmux_name`/`pane_id`/`root_pid`/`root_created_at`), two worker `Session` rows `worker-a` and `worker-b` both with `orchestrator_id: Some("orch-1")`, and a second orphan orchestrator `orch-2` (persisted, no workers) to exercise `Unrelated` between two orchestrators. Construct a matching `PaneIdentity` for the "genuine caller" cases (same `server_epoch`/`physical_tmux_name`/`pane_id`/`pane_pid`/`pane_created_at` as the persisted identity, for `orch-1`; for workers, a `PaneIdentity` whose `physical_tmux_name` equals the worker's own id — exact field values are the test's choice as long as they're internally consistent). Tests, by relationship:

```rust
#[test]
fn direct_orchestrator_when_sender_is_receivers_recorded_parent() {
    // orch-1 (genuine pane) sends to worker-a (orchestrator_id == "orch-1")
    // assert relationship == DirectOrchestrator, verified == true
}

#[test]
fn sibling_when_both_share_the_same_orchestrator() {
    // worker-a (genuine pane) sends to worker-b, both orchestrator_id == "orch-1"
    // assert relationship == Sibling, verified == true
}

#[test]
fn descendant_when_a_worker_messages_its_own_orchestrator() {
    // worker-a (genuine pane) sends to orch-1
    // assert relationship == Descendant, verified == true
}

#[test]
fn unrelated_when_verified_but_no_lineage_recorded() {
    // orch-2 (genuine pane) sends to worker-a (orchestrator_id == "orch-1", not orch-2)
    // assert relationship == Unrelated, verified == true
}

#[test]
fn unrelated_and_unverified_when_claimed_pane_does_not_match() {
    // caller's PaneIdentity.physical_tmux_name is "worker-b" but claimed_sender is "worker-a"
    // assert relationship == Unrelated, verified == false
}

#[test]
fn unverifiable_when_no_caller_pane_is_available() {
    // caller_pane: None, claimed_sender: Some("worker-a")
    // assert relationship == Unverifiable, verified == false
}

#[test]
fn unverifiable_when_no_sender_is_claimed_at_all() {
    // claimed_sender: None
    // assert relationship == Unverifiable, verified == false, claimed_session_id == None
}

#[test]
fn orchestrator_self_registers_on_first_attested_send() {
    // orch-2 has NO persisted OrchestratorRuntimeIdentity yet; caller_pane's
    // physical_tmux_name == "orch-2" and caller_descends_from its pane_pid
    // holds (same migration precondition authorize_orchestrator uses).
    // assert verified == true, and store.orchestrator_runtime_identity("orch-2")
    // now returns Some(..) (the migration path ran).
}
```

Plus `render()` text assertions:

```rust
#[test]
fn render_matches_spec_examples_for_direct_orchestrator_and_unrelated() {
    // DirectOrchestrator, verified: true, id "orch-1" ->
    // "[ninox: sender session orch-1 verified as your direct spawning orchestrator, confirmed via process-ancestry + tmux-pane cross-check]"
    // Unrelated, verified: true, id "orch-2" ->
    // "[ninox: sender session orch-2 verified as an unrelated session — no lineage relationship recorded]"
}

#[test]
fn render_is_generic_and_omits_the_claim_whenever_verified_is_false() {
    // Unverifiable and Unrelated/verified:false both render exactly:
    // "[ninox: sender identity could not be verified]"
}
```

- [ ] **Step 2: Run the new tests to verify they fail to compile (types don't exist yet)**

Run: `cargo test -p ninox-core orchestrator_auth:: 2>&1 | tail -30`
Expected: compile error — `SenderRelationship`/`SenderAttestation`/`attest_sender` not found.

- [ ] **Step 3: Refactor `authorize_orchestrator`'s internals into two reusable private functions**

In `orchestrator_auth.rs`, extract:

```rust
fn persisted_or_migrated_identity(
    store: &Store,
    orchestrator_id: &str,
    runtime: &runtime::PaneIdentity,
) -> Result<Option<OrchestratorRuntimeIdentity>>
```

— move the existing `persisted`/migration block (current lines 32-62) into this function verbatim (same self-registration semantics, same race handling), returning the resolved `persisted` identity (or `None` if it genuinely isn't registered and migration preconditions don't hold).

```rust
fn runtime_identity_matches(persisted: &OrchestratorRuntimeIdentity, runtime: &runtime::PaneIdentity) -> bool
```

— move the existing final `ensure!` condition (current lines 64-71) into this as a plain boolean expression (no `anyhow::ensure!`).

Rewrite `authorize_orchestrator` to call both:

```rust
let persisted = persisted_or_migrated_identity(store, orchestrator_id, runtime)?
    .context("orchestrator has no persisted runtime identity")?;
anyhow::ensure!(
    runtime_identity_matches(&persisted, runtime),
    "caller is not running under the immutable orchestrator runtime"
);
Ok(orchestrator_id.to_string())
```

Its existing public behavior and existing tests (there are none in this file today, but any caller-side tests in `cli.rs`/`spawn`-related tests) must be unaffected — this is a pure refactor.

- [ ] **Step 4: Implement `SenderRelationship`, `SenderAttestation`, and `attest_sender`**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderRelationship { DirectOrchestrator, Sibling, Descendant, Unrelated, Unverifiable }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderAttestation {
    pub claimed_session_id: Option<String>,
    pub relationship: SenderRelationship,
    pub verified: bool,
    basis: &'static str, // only meaningful when verified
}
```

`attest_sender`:
1. `claimed_sender.filter(|s| !s.is_empty())` — `None` → return `{ claimed_session_id: None, relationship: Unverifiable, verified: false, basis: "" }`.
2. `caller_pane` is `None` → return `{ claimed_session_id: Some(claimed), relationship: Unverifiable, verified: false, basis: "" }`.
3. `let is_orch = store.is_orchestrator(claimed).unwrap_or(false);`
4. If `is_orch`: `verified = persisted_or_migrated_identity(store, claimed, pane).ok().flatten().is_some_and(|p| runtime_identity_matches(&p, pane))`, `basis = "process-ancestry + tmux-pane cross-check"`.
5. Else: `verified = pane.physical_tmux_name == claimed`, `basis = "live session-pane identity check"`.
6. If `!verified`: return `{ claimed_session_id: Some(claimed), relationship: Unrelated, verified: false, basis: "" }`.
7. Else compute `relationship_of(store, claimed, receiver_session_id, is_orch)` (new private helper — see below) and return `{ claimed_session_id: Some(claimed), relationship, verified: true, basis }`.

```rust
fn relationship_of(store: &Store, sender: &str, receiver: &str, sender_is_orchestrator: bool) -> SenderRelationship {
    let receiver_session = store.get_session(receiver).ok().flatten();
    let receiver_parent = receiver_session.as_ref().and_then(|s| s.orchestrator_id.as_deref());
    if sender_is_orchestrator && receiver_parent == Some(sender) {
        return SenderRelationship::DirectOrchestrator;
    }
    if !sender_is_orchestrator {
        let sender_session = store.get_session(sender).ok().flatten();
        let sender_parent = sender_session.as_ref().and_then(|s| s.orchestrator_id.as_deref());
        if let Some(parent) = sender_parent {
            if receiver_parent.is_some() && sender_parent == receiver_parent { return SenderRelationship::Sibling; }
            if parent == receiver { return SenderRelationship::Descendant; }
        }
    }
    SenderRelationship::Unrelated
}
```

`render()`:

```rust
pub fn render(&self) -> String {
    let Some(id) = self.claimed_session_id.as_deref().filter(|_| self.verified) else {
        return "[ninox: sender identity could not be verified]".to_string();
    };
    match self.relationship {
        SenderRelationship::DirectOrchestrator => format!(
            "[ninox: sender session {id} verified as your direct spawning orchestrator, confirmed via {}]", self.basis
        ),
        SenderRelationship::Sibling => format!(
            "[ninox: sender session {id} verified as a sibling session under the same orchestrator, confirmed via {}]", self.basis
        ),
        SenderRelationship::Descendant => format!(
            "[ninox: sender session {id} verified as a descendant session (one you spawned, directly or transitively), confirmed via {}]", self.basis
        ),
        SenderRelationship::Unrelated => format!(
            "[ninox: sender session {id} verified as an unrelated session — no lineage relationship recorded]"
        ),
        SenderRelationship::Unverifiable => "[ninox: sender identity could not be verified]".to_string(),
    }
}
```

(`Unverifiable` never reaches the match in practice since `claimed_session_id`/`verified` are already gated above, but keep the arm for exhaustiveness and clarity.)

- [ ] **Step 5: Run the tests from Step 1 to verify they pass**

Run: `cargo test -p ninox-core orchestrator_auth:: 2>&1 | tail -40`
Expected: all new tests PASS.

- [ ] **Step 6: Run clippy on the crate**

Run: `cargo clippy -p ninox-core --all-targets 2>&1 | tail -40`
Expected: clean (fix any warnings before continuing).

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-core/src/orchestrator_auth.rs
git commit -m "feat(messaging): generalize orchestrator_auth into a shared sender-attestation function

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

## Task 2: Wire attestation into `deliver_message`/`deliver_by_mechanism`

**Files:**
- Modify: `crates/ninox-core/src/messaging.rs`

**Interfaces:**
- Consumes: `orchestrator_auth::{attest_sender, SenderAttestation}` from Task 1; `crate::runtime::PaneIdentity`.
- Produces: `deliver_message`'s new signature (for Task 3's callers):
  `pub async fn deliver_message(store: &Store, sessions_dir: &Path, session_id: &str, message: &str, mechanism: SendMechanism, from: Option<&str>, sender_pane: Option<&runtime::PaneIdentity>) -> Result<()>`

- [ ] **Step 1: Write failing tests for attestation annotation in `messaging.rs`**

Extend the existing `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn inbox_delivery_carries_the_attestation_line_in_its_content() {
    // store has orch-1 (persisted OrchestratorRuntimeIdentity) and worker-1
    // (orchestrator_id: Some("orch-1"), claude-code, hooks installed).
    // Build a matching PaneIdentity for orch-1 and pass it as sender_pane,
    // from: Some("orch-1"), session_id: "worker-1".
    // assert the delivered inbox message's `.text` STARTS WITH
    // "[ninox: sender session orch-1 verified as your direct spawning orchestrator"
    // and still ends with the original message text.
}

#[tokio::test]
async fn keystrokes_delivery_carries_the_attestation_line() {
    // Can't easily assert tmux content without a real session; instead
    // assert via a session-socket-style unit test (see next) and a
    // dedicated regression test in Task 4 that exercises all three
    // mechanisms against the SAME annotated-text helper directly:
    // call the (new, private) annotation helper function directly and
    // assert its output, since Keystrokes/SessionSocket both pass the
    // same annotated string through untouched.
}

#[tokio::test]
async fn session_socket_delivery_carries_the_attestation_line() {
    // Reuse the existing `the_session_socket_mechanism_delivers_to_a_listening_peer`
    // fixture, but call `deliver_message` (not `deliver_via_session_socket`
    // directly) with a real `from`/`sender_pane` pair, and assert the bytes
    // received over the socket's JSON frame's `message.content` starts with
    // the rendered attestation line.
}

#[tokio::test]
async fn system_sender_messages_are_never_annotated() {
    // from: Some(messaging::SYSTEM_SENDER), sender_pane: None — assert the
    // delivered inbox message's `.text` is byte-identical to the input
    // message (no "[ninox:" prefix at all).
}

#[tokio::test]
async fn unresolvable_sender_still_gets_the_unverifiable_line() {
    // from: None, sender_pane: None — assert delivered content starts with
    // "[ninox: sender identity could not be verified]".
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core messaging:: 2>&1 | tail -40`
Expected: compile error (new `deliver_message` parameter doesn't exist yet) or assertion failures once it compiles against the old 6-arg signature — update call sites in this same file's existing tests to pass `None` for `sender_pane` as part of this step so the crate compiles, then confirm the NEW tests fail on their assertions.

- [ ] **Step 3: Implement the annotation helper and update `deliver_message`'s signature**

```rust
fn annotate_with_attestation(
    store: &Store,
    receiver: &str,
    message: &str,
    from: Option<&str>,
    sender_pane: Option<&runtime::PaneIdentity>,
) -> String {
    if from == Some(SYSTEM_SENDER) {
        return message.to_string();
    }
    let attestation = crate::orchestrator_auth::attest_sender(store, from, sender_pane, receiver);
    format!("{}\n\n{message}", attestation.render())
}
```

Update `deliver_message` to take the new `sender_pane: Option<&runtime::PaneIdentity>` parameter, build `let annotated = annotate_with_attestation(store, session_id, message, from, sender_pane);`, and pass `&annotated` (not `message`) into `deliver_by_mechanism`. `deliver_by_mechanism`'s own signature is unchanged — it already just takes `message: &str` and doesn't need to know about attestation.

- [ ] **Step 4: Update every existing call to `deliver_message` within `messaging.rs`'s test module to pass `None` for the new parameter**

(The 6 existing test-module calls at lines ~365, 383-384, 395, 414, 436.)

- [ ] **Step 5: Run the tests from Step 1 to verify they pass**

Run: `cargo test -p ninox-core messaging:: 2>&1 | tail -60`
Expected: all PASS, including every pre-existing test (unaffected by the new optional parameter defaulting to `None`).

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p ninox-core --all-targets 2>&1 | tail -40`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-core/src/messaging.rs
git commit -m "feat(messaging): prepend the sender attestation line to delivered message content

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

## Task 3: Thread live caller pane identity through every production call site

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs:882` (`Command::Send` arm)
- Modify: `crates/ninox-ui-shared/src/cli.rs:1764` (`run_spawn_orchestrator`'s initial-brief send)
- Modify: `crates/ninox-core/src/events.rs:512` (`Engine::send_to_session`)
- Modify: `crates/ninox-ui-shared/src/fleet.rs:454` (`deliver_briefing`)
- Modify: `crates/ninox-ui-shared/src/restart.rs:285` (relaunch restart note)

**Interfaces:**
- Consumes: `deliver_message`'s new signature from Task 2; `ninox_core::runtime::current_private_pane_identity()` (existing, already used by `cli.rs`'s `Command::Spawn` arm at line 844).

- [ ] **Step 1: `cli.rs`'s `Command::Send` arm — capture and thread the real caller pane**

Immediately before the existing `deliver_message` call (`cli.rs:882-885`), add:

```rust
let sender_pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
```

and pass `sender_pane.as_ref()` as the new last argument to `deliver_message`. This is the one call site where attestation actually matters end-to-end: the calling process IS the sender's own shell, invoked synchronously, so its live pane identity is exactly what needs cross-checking against the claimed `from`.

- [ ] **Step 2: `cli.rs`'s `run_spawn_orchestrator` initial-brief send — same pattern**

At `cli.rs:1764`, before the `deliver_message` call, capture `let sender_pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();` and pass `sender_pane.as_ref()`. `spawner` here is resolved from `NINOX_ORCHESTRATOR_ID` (a spoofable env var, per the existing comment at `cli.rs:1754`) — attesting it closes the same class of gap this spec targets, and it already flows through the exact same `deliver_message` function, so this is a natural, low-cost extension rather than new scope.

- [ ] **Step 3: `events.rs:512`, `fleet.rs:454`, `restart.rs:285` — pass `None`**

All three already pass `from: Some(SYSTEM_SENDER)`; add `None` as the new trailing argument. `annotate_with_attestation` (Task 2) short-circuits on `SYSTEM_SENDER` before ever looking at `sender_pane`, so `None` is correct and these messages remain unannotated, exactly as today.

- [ ] **Step 4: Build the full workspace**

Run: `cargo build --workspace 2>&1 | tail -60`
Expected: clean build, no errors from the signature change propagating.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-ui-shared/src/cli.rs crates/ninox-core/src/events.rs crates/ninox-ui-shared/src/fleet.rs crates/ninox-ui-shared/src/restart.rs
git commit -m "feat(messaging): capture live caller pane identity at every deliver_message call site

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

## Task 4: Integration test — mock orchestrator/worker pair, tamper case, and the three-mechanism regression test

**Files:**
- Modify: `crates/ninox-core/src/messaging.rs` (additional tests in the existing test module), OR
- Create: `crates/ninox-core/tests/sender_attestation_integration.rs` if the fixture needs more than `messaging.rs`'s existing test helpers provide (judgment call for the implementer — prefer extending `messaging.rs`'s module unless the mock orchestrator+worker setup needs machinery not already in scope there).

**Interfaces:**
- Consumes: `deliver_message`, `orchestrator_auth::{attest_sender, SenderRelationship}`, `Store` test-fixture helpers already present in `messaging.rs`'s test module (`session_with`, `write_installed_hooks`).

- [ ] **Step 1: Write the end-to-end "real orchestrator → worker" test**

```rust
#[tokio::test]
async fn orchestrator_sending_its_own_worker_a_message_is_verified_and_relationship_direct() {
    // Store: register orchestrator "orch-1" (store.upsert_orchestrator + a
    // persisted OrchestratorRuntimeIdentity matching a constructed PaneIdentity),
    // and worker-1 with orchestrator_id: Some("orch-1"), claude-code, hooks installed.
    // deliver_message(..., "worker-1", "do the thing", SendMechanism::Inbox,
    //                  Some("orch-1"), Some(&pane_identity))
    // Read back the inbox message; assert:
    //   - .from == Some("orch-1".to_string())  (existing structured field, unchanged)
    //   - .text starts with the DirectOrchestrator rendering from Task 1
    //   - .text ends with "do the thing"
}
```

- [ ] **Step 2: Write the tamper test**

```rust
#[tokio::test]
async fn tampering_with_the_claimed_sender_is_rejected() {
    // Same fixture as Step 1, but pass a PaneIdentity whose physical_tmux_name
    // is "worker-1" (or anything NOT "orch-1") while still claiming
    // from: Some("orch-1").
    // Assert the delivered content is exactly
    // "[ninox: sender identity could not be verified]\n\ndo the thing"
    // — i.e. verified: false / Unrelated per attest_sender, never rendered
    // as DirectOrchestrator.
    //
    // Also call attest_sender directly in this test (not just through
    // deliver_message) to assert the structured result:
    //   relationship == SenderRelationship::Unrelated, verified == false
}
```

- [ ] **Step 3: Write the three-mechanism regression test**

```rust
#[tokio::test]
async fn all_three_mechanisms_carry_the_attestation_line() {
    // One shared (from, sender_pane, receiver) triple that verifies as
    // DirectOrchestrator. Exercise:
    //   - SendMechanism::Inbox against a worker with installed hooks
    //     (assert inbox::read_pending_messages(...).text starts with the line)
    //   - SendMechanism::SessionSocket against a listening unix-socket peer
    //     (reuse the existing socket-listener test pattern; assert the JSON
    //     frame's message.content starts with the line)
    //   - SendMechanism::Keystrokes — assert indirectly: since send_keys
    //     against a nonexistent tmux session errors, instead unit-test
    //     `annotate_with_attestation` directly (now `pub(crate)` if not
    //     already) with the same inputs and assert its output matches the
    //     line used in the other two assertions, proving all three paths
    //     go through the identical annotation step before mechanism dispatch.
}
```

(If `annotate_with_attestation` needs to go from private to `pub(crate)` to support this test, make that change in this task, not Task 2, since Task 2's own tests didn't need it.)

- [ ] **Step 4: Run the full test suite for the crate**

Run: `cargo test -p ninox-core 2>&1 | tail -80`
Expected: all PASS, including every test from Tasks 1-4.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/messaging.rs
git commit -m "test(messaging): integration coverage for sender attestation across all delivery mechanisms

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

## Task 5: Full workspace verification and regression check against live flows

**Files:** none (verification-only task).

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace 2>&1 | tail -60`
Expected: clean.

- [ ] **Step 2: Full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -150`
Expected: all PASS — specifically confirm `ninox-core`'s `orchestrator_auth`/`messaging` modules, `ninox-ui-shared`'s `cli`/`fleet`/`restart` test modules, and anything exercising `ninox plan register`/`ninox reap`/spawn-auth (PR #200's tests) are unaffected by the `authorize_orchestrator` refactor in Task 1.

- [ ] **Step 3: Full workspace clippy**

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -100`
Expected: clean.

- [ ] **Step 4: Manual regression check of `authorize_orchestrator`'s refactor**

Since `persisted_or_migrated_identity`/`runtime_identity_matches` are extracted but must behave byte-identically to the pre-refactor inline code, diff-review the extracted functions against the original `orchestrator_auth.rs` (available via `git show origin/v1:crates/ninox-core/src/orchestrator_auth.rs`) to confirm no behavioral drift — same field comparisons, same `.abs() <= 2_000` tolerance, same race-handling fallback re-read on a lost `register_migrated_orchestrator_runtime` race.

- [ ] **Step 5: Confirm no regression to `ninox send`'s existing CLI contract**

Run a quick manual trace (no live tmux session needed): read `cli.rs`'s `Command::Send` arm post-change and confirm the command's exit behavior (success/failure, error messages) is unchanged for every pre-existing case — only the delivered *content* changes (gains a prepended line); `deliver_message`'s `Result<()>` contract, the `record_message_delivered` counter call, and all fallback-to-keystrokes behavior in `deliver_by_mechanism` are untouched by this plan (verified by re-reading `messaging.rs` Task 2's diff against the original).

- [ ] **Step 6: No commit needed (verification only) — proceed to PR**

---

## Self-Review Notes

- **Spec coverage:** Attestation generalization (Task 1), wiring into `deliver_message`/`deliver_by_mechanism` for all three mechanisms (Task 2), delivered-message-format examples reproduced verbatim where the spec gives them (Task 1 Step 4 `render()`), unit tests per relationship category (Task 1), integration test with tamper case (Task 4), three-mechanism regression test (Task 4), full build/test/clippy gate (Task 5) — all five Design/Testing subsections of the spec map to a task.
- **Type consistency:** `SenderAttestation`/`SenderRelationship`/`attest_sender` signatures introduced in Task 1 are used identically in Tasks 2 and 4; `deliver_message`'s new parameter name (`sender_pane: Option<&runtime::PaneIdentity>`) is consistent across Tasks 2 and 3.
- **Review Focus:** all five listed concerns (tamper, no-pane-available, `from: None`, first-use orchestrator self-registration, per-mechanism regression) each have an owning test in Task 1 or Task 4.
