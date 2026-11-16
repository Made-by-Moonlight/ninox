# Ninox Prompt Routing Implementation Plan

> **For agentic workers:** this plan is executed natively (no human reviewer
> available in this session — an autonomous worker task). Implemented
> directly, task by task, committing after each.

**Goal:** Wire up the TUI's already-scaffolded "ninox prompt" (bottom of
the Graph tab's graph column, always visible — see
`docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`'s
"The ninox prompt" section) so that submitting it actually does something,
instead of `graph_prompt_key`'s current `"not yet wired: <input>"` stub.

**Spec:** `docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`
("The ninox prompt" section). **Ticket:** MLOPS-5312.

## Starting point (verified against current `v1`, commit `fbaf7e9`)

`docs/superpowers/plans/2026-10-23-fleet-overview-phase1-plan.md` (Task 5)
already shipped the whole widget shell: `GraphLayout.prompt: Rect` (docked
under the canvas, `layout.rs::graph()`), `GraphState.prompt_input`/
`prompt_editing` (`graph.rs`), the `/`-to-focus / type / Esc / Enter key
handling (`state.rs::graph_prompt_key`), and the rendering (`view.rs::
draw_graph_prompt`). Phase 1 explicitly deferred "real ninox-prompt NLU
wiring" as follow-on work (its Task 6) — that follow-on is this ticket.

## Scope decision (spec's own open question)

The spec leaves "how the ninox prompt disambiguates an action request from
a read query" as an open question "left to the implementation plan." The
full answer needs natural-language interpretation (what counts as an
imperative, multi-section planning, worker-type drafting) that the spec's
own non-goals explicitly exclude from this line of work ("the auto-
planning/sectioning *algorithm*... is a separate spec"). Building an NLU
layer in Rust inside the TUI would both violate that non-goal and massively
exceed "keep the change localized."

**Resolution:** a ninox orchestrator is itself a natural-language-capable
agent (a running Claude Code / Codex / etc. session) — the party actually
positioned to interpret "what's blocking the webhook rewrite?" or "spawn an
orchestrator for the billing migration" is the orchestrator receiving the
prompt, not the Rust TUI process. So "routed to the orchestrator" is
implemented literally: the typed text is delivered as an inbox/session
message to a resolved target orchestrator, using the exact same delivery
path `ninox send` already uses
(`ninox_core::messaging::deliver_message_with_acp`). The orchestrator's own
reasoning (today, manually, same as any inter-session message) is the NLU;
no keyword/intent matching is added to the TUI.

This also resolves the spec's "answers appear...by highlighting or
selecting nodes" clause for free: when the orchestrator *acts* on the
prompt (spawns a worker, messages another session), the already-shipped
live-flash/node/tree mechanisms (`graph-view-live-comms`,
`a2140a3`) surface that without any new code here. The "inline in the
prompt area" half is implemented as a synchronous delivery
confirmation/failure line (not a round-tripped AI answer — there is no
reply channel from an orchestrator back to "the TUI" today, since the TUI
is not itself an addressable session; building one is a separate,
larger follow-on, requested at the end of this plan).

**Target resolution** (no node selected → ambiguous; one of several
orchestrators → must disambiguate by selection — consistent with the
spec's "no destructive/spawning action without...an unambiguous [target]"
spirit):
- Selected node is a ghost → its plan's real parent orchestrator
  (`GhostNode.parent_id`) — never the ghost itself, per the spec's "a
  ghost...can never be a communication-edge endpoint" invariant.
- Selected node is a real orchestrator → itself.
- Selected node is a real worker → its owning orchestrator (`Row.group`).
- Nothing selected, exactly one orchestrator running → that one.
- Nothing selected, zero or multiple orchestrators → refuse locally with a
  message asking the user to select a node first (shown inline in the
  prompt area, same as a delivery failure); no message is sent.

## Architecture

- `ninox-tui/src/tui/graph.rs`: `GraphState` gains `prompt_answer:
  Option<String>` — the last inline status/error, shown in place of the
  placeholder until the next `/`-edit clears it.
- `ninox-tui/src/tui/state.rs`: new `Action::SendPrompt { target: String,
  message: String }`; `graph_prompt_key`'s `Enter` arm resolves a target
  (new pure `prompt_target(&TuiState) -> Result<String, String>` helper,
  unit-testable without any I/O) and either returns the action or sets
  `prompt_answer` to the refusal reason.
- `ninox-tui/src/tui/mod.rs`: `perform()` handles `Action::SendPrompt` by
  spawning the same delivery call `cli.rs`'s `Command::Send` arm already
  makes (`deliver_message_with_acp`, with an `HttpAcpRouter` so an
  ACP-backed orchestrator is reached correctly, `from` resolved via
  `worker_status::resolve_session_id` exactly like the CLI — `None` for a
  TUI run directly by a human, same as `ninox send` outside any session).
  The result comes back over a new `prompt_tx`/`prompt_rx` channel
  (mirroring the existing `uncommitted_tx`/`op_tx` pattern already in this
  file) and sets `GraphState.prompt_answer`.
- `ninox-tui/src/tui/view.rs`: `draw_graph_prompt` shows `prompt_answer`
  when present and not editing; footer hint for the editing state changes
  from "not yet wired" to "send"; `HELP`'s Graph-tab section gains the `/`
  entry.

No changes to `ninox-overview` (node/tree/controls model) or to ghost-plan
review (`a`/`s`/`+`/`d` keys) — those remain the separate, already-tracked
follow-on ("real ninox-prompt NLU wiring" is this ticket; "real auto-
planning" is a different one, per the phase-1 plan's Task 6 follow-on
list). Keeping this change inside `graph.rs`/`state.rs`/`view.rs`/`mod.rs`
(prompt-specific code paths only) avoids touching the node-rendering and
worker-types-screen code other workers are changing in parallel.

## Review focus

- A ghost node selected when the prompt is submitted must route to its
  `parent_id`, never attempt to message the ghost's own (non-existent)
  session id.
- Zero or 2+ orchestrators with nothing selected must refuse locally
  (no send attempt, no panic indexing an empty/ambiguous list).
- A stale selection (id no longer in `st.rows`, e.g. the session just
  ended) must not panic — fall back to sending to the raw id rather than
  crashing on a failed lookup.
- `prompt_answer` must only ever be read while `!prompt_editing` — opening
  the prompt again (`/`) clears it so a stale answer never bleeds into a
  fresh composition.

---

## Tasks

### Task 1: `prompt_target` resolution + `Action::SendPrompt`

**Files:** `crates/ninox-tui/src/tui/state.rs`, `crates/ninox-tui/src/tui/graph.rs`.

- [ ] `graph.rs`: add `GraphState.prompt_answer: Option<String>`; update the
      doc comment on `prompt_input`/`prompt_editing` to drop the "Phase 1...
      not yet wired" language.
- [ ] `state.rs`: add `Action::SendPrompt { target: String, message: String }`.
- [ ] `state.rs`: add `fn prompt_target(st: &TuiState) -> Result<String, String>`
      implementing the resolution rules above, using `st.graph.overview.
      selected_node()` and `st.rows`.
- [ ] Tests (pure, no I/O): ghost → parent id; real orchestrator selected →
      itself; real worker selected → its `group`; worker with no group
      (standalone) → falls back to its own id; nothing selected + exactly
      one orchestrator → that id; nothing selected + zero orchestrators →
      `Err`; nothing selected + two orchestrators → `Err`; a selected id with
      no matching row → falls back to the raw id (no panic).
- [ ] Commit: `feat(tui): resolve the ninox prompt's target orchestrator`.

### Task 2: Wire `graph_prompt_key`'s submit path

**Files:** `crates/ninox-tui/src/tui/state.rs`.

- [ ] Rewrite `graph_prompt_key`: `Enter` trims+takes `prompt_input`; empty
      → `Action::None` (unchanged behavior); otherwise calls `prompt_target`
      and either returns `Action::SendPrompt{..}` or sets `prompt_answer` to
      the refusal and returns `Action::None`. `Esc`/typing unchanged except
      each arm now returns `Action` directly instead of falling through.
- [ ] `KeyCode::Char('/')` (open prompt) also clears `prompt_answer`.
- [ ] Tests: Enter with a resolvable target returns the right `Action::
      SendPrompt` and clears `prompt_input`/`prompt_editing`; Enter with an
      unresolvable target sets `prompt_answer` and returns `Action::None`;
      Enter with blank/whitespace-only input does neither (matches today's
      "nothing happens" behavior); re-opening the prompt after an answer was
      set clears it; the existing `typing_a_digit_into_the_graph_prompt_
      does_not_switch_tabs` test keeps passing unmodified.
- [ ] Commit: `feat(tui): submit the ninox prompt as a message to its target orchestrator`.

### Task 3: Deliver it

**Files:** `crates/ninox-tui/src/tui/mod.rs`.

- [ ] Add `prompt_tx: tokio::sync::mpsc::UnboundedSender<Result<String, String>>`
      to `Loop`; create the channel and its `prompt_rx` in `event_loop`
      alongside the existing ones; add a `tokio::select!` arm that sets
      `st.graph.prompt_answer` (and still calls `st.notify` for the existing
      global notice, matching how other async results surface) and marks
      the frame dirty.
- [ ] `perform()` gains `Action::SendPrompt { target, message }`: resolve the
      target's display name from `st.rows` for the messages below, set
      `prompt_answer` to an immediate "sending…" line, then spawn a task
      that resolves `sessions_dir`/`from`/`sender_pane` exactly like `cli.rs`'s
      `Command::Send` arm and calls `ninox_core::messaging::
      deliver_message_with_acp` with an `HttpAcpRouter { port }` (`port`
      threaded into `perform()` as a new parameter from `event_loop`), then
      sends `Ok("→ sent to <name>")` or `Err("send to <name> failed: ...")`
      over `prompt_tx`.
- [ ] No unit test for the spawned task's networked body itself (matches
      this file's existing convention — `Action::Spawn`, `Action::Kill`
      etc. are also untested at that level); covered by Task 1/2's pure
      resolution tests plus a manual smoke test (see Task 4).
- [ ] Commit: `feat(tui): deliver the ninox prompt to its resolved orchestrator`.

### Task 4: Rendering, hints, help text, and manual verification

**Files:** `crates/ninox-tui/src/tui/view.rs`.

- [ ] `draw_graph_prompt`: show `prompt_answer` (styled like normal ink, not
      faint) when set and not editing, instead of the placeholder; update
      its doc comment.
- [ ] Footer hint for `(_, View::Graph, _) if st.graph.prompt_editing`:
      `"not yet wired"` → `"send"`.
- [ ] `HELP`'s "Graph tab (bare keys)" section gains a `("/", "ask or act —
      sends to the selected node's orchestrator")`-style entry.
- [ ] Test: a `TestBackend` render (same style as this file's existing
      `board_draws_sidebar_rows...` tests) with `st.graph.prompt_answer =
      Some("→ sent to acme".into())` and `prompt_editing = false` shows that
      text in the Graph tab instead of the placeholder.
- [ ] `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo test --workspace`.
- [ ] Manual smoke test: run two local sessions (an orchestrator + a
      worker), open `nx`'s Graph tab, select the orchestrator, type a prompt,
      confirm it's delivered (`ninox brain`/session inbox or the receiving
      pane shows it) and the prompt area shows "→ sent to...".
- [ ] Commit: `feat(tui): render the ninox prompt's inline answer`.

### Task 5: Follow-on requests

- [ ] `ninox request-work` for: a real reply channel so an orchestrator's
      *answer* (not just its resulting actions) can round-trip back into
      the prompt area, rather than only a delivery confirmation; the GUI
      (Iced) equivalent of this same wiring, once the GUI gets its own
      four-region screen.
