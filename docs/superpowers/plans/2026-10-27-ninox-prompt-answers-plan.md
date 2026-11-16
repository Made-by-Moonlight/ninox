# Ninox Prompt Answers Implementation Plan

> **For agentic workers:** this plan is executed natively (no human reviewer
> available in this session — an autonomous worker task). Implemented
> directly, task by task, committing after each.

**Goal:** give the orchestrator a reply path back into the TUI's "ninox
prompt" (`docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`,
"The ninox prompt" section), so a read-query answer renders inline in the
prompt area or by highlighting the relevant node, instead of only ever
showing PR #237's delivery confirmation ("→ sent to X").

**Spec:** `docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`
("The ninox prompt"). **Ticket:** MLOPS-5319. Builds on
`docs/superpowers/plans/2026-10-27-ninox-prompt-routing-plan.md` (PR #237,
merged).

## Starting point (verified against `origin/v1`, commit `6eefb9d`)

PR #237 wired `GraphState.prompt_answer`/`Action::SendPrompt`/
`prompt_target` so a submitted prompt is delivered as a session message to
its resolved orchestrator, with the delivery outcome ("→ sent to X" /
"send to X failed: …") shown inline. It explicitly deferred "a real reply
channel so an orchestrator's *answer* ... can round-trip back into the
prompt area" as follow-on work — this plan is that follow-on.

## Scope decisions

1. **Reply transport.** The TUI is a standalone process that polls
   `ninox-core`'s `Store` directly (confirmed: no SSE/event-bus consumption
   anywhere in `ninox-tui`) and has no addressable session identity of its
   own (same constraint PR #237's plan already documented). The existing
   cross-process delivery mechanism for *inter-session* messages is the
   file-based inbox (`ninox_core::inbox`) — written by one process, read by
   another, atomic tmp-then-rename. This plan adds a parallel, much
   simpler file-based channel in the same style: one JSON file per
   outstanding prompt at `{sessions_dir}/prompt_replies/{reply_to}.json`,
   written by a new `ninox prompt-answer` CLI command and consumed (read,
   then deleted) by the TUI's existing 1s tick — no new channel/task
   needed, no `ninox-server`/HTTP dependency.
2. **Correlation.** Each outgoing `Action::SendPrompt` carries a fresh
   `reply_to` id (`ninox_core::prompt_reply::new_reply_id`, a UUIDv4,
   matching the `Uuid::new_v4()` convention already used in
   `config.rs`/`harness.rs`/`brain_harvest.rs`). The delivered message text
   is extended with a short block telling the orchestrator the exact
   `ninox prompt-answer <reply_to> ...` command to run. `GraphState` tracks
   the single outstanding `prompt_pending_id`; reopening the prompt (`/`)
   abandons it (same "stale answer must never bleed into a fresh
   composition" invariant `prompt_answer` already has) — a reply for an
   abandoned id is simply never read back (the file is left on disk,
   harmless, cleaned up by nothing today; acceptable for a first cut, not
   a correctness problem since nothing currently sweeps `prompt_replies/`,
   same as how `inbox`'s own entries fall out of relevance once consumed).
3. **Node highlighting is single-select.** The spec says "node(s)"
   (plural), but `ninox-overview::OverviewState` only models a single
   `selected: NodeId` today, used throughout the detail panel/controls.
   Extending it to a real multi-select is a larger, shared (TUI+GUI)
   change outside this ticket's "keep the change localized" scope (and
   risks conflicting with the other `v1` workers currently touching
   `ninox-overview`/`graph.rs`/`state.rs` in parallel). `--select` accepts
   multiple ids; the first is actually selected, the rest are named in the
   inline text (`"… (+N more)"`). Noted here as the spec's open-question
   resolution, not re-litigated.
4. **Action-request gating (single-worker-executes / epic-becomes-ghost-
   plan) is explicitly out of scope for this PR.** Confirmed by reading
   the current ghost-review code (`state.rs`'s `graph_key`): the `a`/`s`
   approve/discard keys are *already* stubs ("not yet wired to a
   planner"), and every comment pointing at them names the same blocker —
   the auto-planning algorithm (spec item 9, `docs/superpowers/specs/
   2026-10-22-auto-planning-design.md`, ticket MLOPS-5309, in flight as a
   separate `v1` worker). Wiring a real epic→ghost-plan path means wiring
   real "Approve & spawn", which doesn't exist yet independent of this
   ticket. Per this ticket's own instructions: do the answer/highlight
   path (this plan) and `ninox request-work` the gating once MLOPS-5309
   lands. "A single worker executes immediately" needs no new plumbing at
   all — the orchestrator already has `spawn`/`spawn-orchestrator`; it can
   just act on a received prompt and reply with a confirmation through the
   same channel this plan adds.
5. **Worker-type drafting** (also named in the Linear ticket's broader
   description, not in this task's own instructions) is likewise left out:
   the worker-types editor screen itself is still an in-flight separate
   `v1` worker's PR (not yet merged), so there is nothing to pre-fill yet.

## Architecture

- `ninox-core/src/prompt_reply.rs` (new): `PromptReply { answer:
  Option<String>, select: Vec<String> }`, `write_reply`/`take_reply`
  (atomic tmp-then-rename write, best-effort consume — a missing or
  malformed file is `Ok(None)`, mirroring `inbox::read_pending_messages`'s
  posture toward a corrupt entry), `new_reply_id() -> String`. Registered
  in `lib.rs`.
- `ninox-tui/src/tui/graph.rs`: `GraphState` gains `pub prompt_pending_id:
  Option<String>`.
- `ninox-tui/src/tui/state.rs`: `Action::SendPrompt` gains a `reply_to:
  String` field; `graph_prompt_key`'s `Enter` arm generates one via
  `ninox_core::prompt_reply::new_reply_id` and sets `prompt_pending_id`;
  the `/`-reopen arm also clears `prompt_pending_id`. New pure function
  `apply_prompt_reply(rows: &[Row], overview: &mut OverviewState, reply:
  PromptReply) -> Option<String>` — selects the first `select` id (if
  any) and returns the inline text to show, unit-testable without the
  event loop.
- `ninox-tui/src/tui/mod.rs`: the `SendPrompt` arm appends the reply
  instructions (with `reply_to`) to the delivered message text. `event_loop`
  resolves `sessions_dir` once up front (same `NINOX_DATA_DIR` convention
  already inline in the `SendPrompt` arm) and the existing 1s tick calls
  `ninox_core::prompt_reply::take_reply` when `st.graph.prompt_pending_id`
  is `Some`, applying it via `state::apply_prompt_reply` and clearing
  `prompt_pending_id`.
- `ninox-ui-shared/src/cli.rs`: new `Command::PromptAnswer { reply_to:
  String, answer: Option<String>, select: Vec<String> }` (comma-delimited
  `--select`), fast-path short-circuited in `dispatch()` like
  `Inbox`/`Whoami` (no store, no tmux) — validates at least one of
  `answer`/`select` is given, then `prompt_reply::write_reply`.
- `ninox-core/src/capabilities.rs` + `ninox-core/skills/orchestrator/
  ninox-prompt-reply.md` (new): orchestrator-only capability documenting
  the reply convention (seeded automatically via the existing registry
  machinery — no hand-edit of `orchestrator_root.rs`/AGENTS.md needed).

## Review focus

- A reply with neither `answer` nor `select` must be rejected by the CLI
  (nothing useful to show), not silently written as a no-op file.
- `take_reply` must treat a missing file as the common case (not yet
  replied — every tick, for however long the orchestrator takes) and a
  malformed one as "nothing useful arrived yet", never an error that
  interrupts the TUI's render loop.
- Reopening the prompt (`/`) after sending must abandon the previous
  `prompt_pending_id` — a reply that arrives after the user has already
  moved on to a new question must never overwrite the new composition's
  state.
- A `--select` id that doesn't match any current row/node must not panic
  (`OverviewState::select` already tolerates an unknown id; this just
  confirms the reply path doesn't add its own lookup that could).
- Two prompts submitted in the same process must get distinct `reply_to`
  ids (UUIDv4 collision is not a realistic risk, but the generator itself
  must actually produce a fresh value each call, not something
  accidentally memoized).

---

## Tasks

### Task 1: `prompt_reply` module

**Files:** `crates/ninox-core/src/prompt_reply.rs` (new),
`crates/ninox-core/src/lib.rs`.

- [x] Write `PromptReply { answer: Option<String>, select: Vec<String> }`
      (`Debug, Clone, Default, PartialEq, Serialize, Deserialize`), with
      `is_empty(&self) -> bool`.
- [x] Write `write_reply(sessions_dir: &Path, reply_to: &str, reply:
      &PromptReply) -> anyhow::Result<()>` (tmp-then-rename, mirroring
      `inbox::write_message`) and `take_reply(sessions_dir: &Path,
      reply_to: &str) -> anyhow::Result<Option<PromptReply>>` (read +
      remove; `NotFound` → `Ok(None)`; a parse failure → `Ok(None)`, never
      `Err`).
- [x] Write `new_reply_id() -> String` (`uuid::Uuid::new_v4().to_string()`).
- [x] Tests: `take_reply` before any write is `None`; write-then-take
      round-trips and consumes (a second `take_reply` is `None`); a
      malformed file is ignored (`None`), not an error; `is_empty` true
      only with neither field set; two `new_reply_id()` calls differ.
- [x] Register `pub mod prompt_reply;` in `lib.rs`.
- [x] Commit: `feat(core): add the ninox-prompt reply file channel`.

### Task 2: `GraphState`/`Action` plumbing + `apply_prompt_reply`

**Files:** `crates/ninox-tui/src/tui/graph.rs`,
`crates/ninox-tui/src/tui/state.rs`.

- [x] `graph.rs`: add `pub prompt_pending_id: Option<String>` to
      `GraphState`, with a doc comment pointing at `state.rs`'s
      `graph_prompt_key`/`apply_prompt_reply`.
- [x] `state.rs`: `Action::SendPrompt` gains `reply_to: String`.
- [x] `graph_prompt_key`'s `Enter` arm: on a resolved target, generate
      `reply_to` via `ninox_core::prompt_reply::new_reply_id()`, set
      `st.graph.prompt_pending_id = Some(reply_to.clone())`, return
      `Action::SendPrompt { target, message, reply_to }`.
- [x] The `/`-reopen arm also sets `st.graph.prompt_pending_id = None`.
- [x] Add `pub(crate) fn apply_prompt_reply(rows: &[Row], overview: &mut
      ninox_overview::state::OverviewState, reply:
      ninox_core::prompt_reply::PromptReply) -> Option<String>`: selects
      `reply.select.first()` into `overview` if present; returns `None`
      for an empty reply, otherwise the inline text combining `answer`
      (if any) and the selected node's name (looked up in `rows`, falling
      back to the raw id) with a `"(+N more)"` suffix when `select.len() >
      1`.
- [x] Tests: existing `graph_prompt_enter_with_a_resolvable_target_sends_
      and_clears_the_input` updated to destructure `Action::SendPrompt`
      (ignoring the generated `reply_to` beyond asserting it's non-empty)
      and to assert `st.graph.prompt_pending_id == Some(reply_to)`;
      `reopening_the_prompt_clears_a_previous_answer` extended to also
      seed and assert-cleared `prompt_pending_id`; new tests for
      `apply_prompt_reply`: answer-only, select-only (one id), select-only
      (multiple ids → "+N more"), answer+select combined, empty reply →
      `None`, unknown select id falls back to the raw id instead of
      panicking.
- [x] Commit: `feat(tui): track the ninox prompt's outstanding reply id`.

### Task 3: Deliver the reply instructions, consume replies on the tick

**Files:** `crates/ninox-tui/src/tui/mod.rs`.

- [x] `Action::SendPrompt` arm: destructure `reply_to`; build the
      delivered message as the original text plus a block telling the
      orchestrator the exact `ninox prompt-answer <reply_to> "<answer>"`
      (and `--select <node-id>[,...]`) command to run, with a pointer to
      the `ninox-prompt-reply` skill; send that extended text (not the
      raw `message`) via `deliver_message_with_acp`.
- [x] `event_loop`: resolve `sessions_dir` once before the `loop {}` body
      (same `NINOX_DATA_DIR` fallback already inline in the `SendPrompt`
      arm).
- [x] The existing `tick.tick()` arm: when `st.graph.prompt_pending_id` is
      `Some(id)`, call `ninox_core::prompt_reply::take_reply(&sessions_dir,
      &id)`; on `Ok(Some(reply))`, call `state::apply_prompt_reply(&st.rows,
      &mut st.graph.overview, reply)` and set `st.graph.prompt_answer` to
      its result (when `Some`), then clear `prompt_pending_id`.
- [x] No unit test for the spawned-task body itself (unchanged convention
      from PR #237's plan — this file's async glue is covered by a manual
      smoke test, Task 5) beyond what Task 2's pure tests already cover.
- [x] Commit: `feat(tui): consume the orchestrator's ninox-prompt reply`.

### Task 4: `ninox prompt-answer` CLI + orchestrator skill

**Files:** `crates/ninox-ui-shared/src/cli.rs`,
`crates/ninox-core/src/capabilities.rs`,
`crates/ninox-core/skills/orchestrator/ninox-prompt-reply.md` (new).

- [x] `cli.rs`: add `Command::PromptAnswer { reply_to: String, answer:
      Option<String>, #[arg(long, value_delimiter = ',')] select:
      Vec<String> }` next to `Send`.
- [x] `dispatch()`: fast-path short-circuit (alongside
      `Inbox`/`IdentityHook`) calling a new `run_prompt_answer(reply_to:
      &str, answer: Option<String>, select: Vec<String>) ->
      anyhow::Result<()>` — rejects (`anyhow::ensure!`) a reply with
      neither field set, otherwise resolves `sessions_dir` (same
      `NINOX_DATA_DIR` convention as `Send`) and calls
      `prompt_reply::write_reply`.
- [x] Tests: `run_prompt_answer` errors when both `answer` and `select`
      are empty; a successful call's write is readable back via
      `prompt_reply::take_reply` with the expected fields.
- [x] Write `ninox-prompt-reply.md` (orchestrator-only capability,
      `{{NINOX_BIN}}` placeholders, same frontmatter/style as
      `spawn-worker.md`): what a ninox-prompt message looks like, how to
      reply with answer text and/or `--select`, and a short note that
      epic/multi-section requests aren't wired to real plan-review
      spawning yet — use `request-work` for those until MLOPS-5309 lands.
- [x] Register the capability in `capabilities.rs`'s `REGISTRY`
      (`Audience::Orchestrator`, `enabled: |_| true`).
- [x] `cargo fmt`, `cargo clippy --workspace --all-targets -- -D
      warnings`, `cargo test --workspace`.
- [x] Manual smoke test: run an orchestrator + worker locally, open the
      TUI's Graph tab, select the orchestrator, send a prompt, and from
      the orchestrator's own shell run `ninox prompt-answer <reply_to>
      "test answer" --select <some-id>` using the `reply_to` printed in
      the message it received; confirm the TUI's prompt area shows the
      answer text and the node becomes selected.
- [x] Commit: `feat(cli): add ninox prompt-answer and its orchestrator skill`.

### Task 5: Follow-on request-work

- [ ] `ninox request-work` for: wiring `a`/`s` (approve/discard) to real
      spawning once MLOPS-5309's auto-planning backend lands, so an
      epic-shaped prompt can route through the existing ghost-node
      review UI with a real plan instead of the `demo_plan` stub; and the
      GUI (Iced) equivalent of this whole reply/answer plumbing.
