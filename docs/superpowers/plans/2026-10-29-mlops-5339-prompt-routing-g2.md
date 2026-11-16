# G2: GUI Prompt Routing + Reply Strip (MLOPS-5339, round-1 try-out feedback)

**Goal:** "The top level ninox prompt doesn't actually do anything" / "I
asked it to spawn some orchestrators and it did not reply." Today
`Message::NinoxPromptSubmitted` just clears the draft. Ship real delivery
and feedback, per `FEEDBACK-v1-tryout-round1.md`'s G2 item (updated 29
Oct): default target is a dedicated **prompt agent** (MLOPS-5415, PR #263,
not yet merged), with a target chip to pick an orchestrator directly
instead, a reply strip above the prompt bar, and a clear-context control.

**Status: both delivery paths are fully real.** MLOPS-5415 (PR #263) merged;
`app.rs` calls `ninox_ui_shared::prompt_agent::ask`/`clear` directly —
`crates/ninox-gui/src/prompt_agent.rs` no longer wraps them (the pre-merge
client-side stub and its orchestrator-fallback path, described in an
earlier revision of this doc, were removed once the real module landed).

- **Directly-picked orchestrator** delivery: the same
  `ninox_core::messaging::deliver_message` call `FleetSendMessageSubmit`
  already uses, and the reply strip's live stream is `ninox_core::runtime
  ::read_screen(id, Some(6), false)` polled on the existing 3s
  `PollSessions` tick (no new subscription) — the same source `ninox read`
  uses. Per PR #268 (`messaging::HUMAN_SENDER` +
  `orchestrator_auth::resolve_live_caller_session_id`), this arm resolves
  the caller's live pane identity and attaches `Some(&from)` (`HUMAN_SENDER`
  when the resolution finds no managed ancestor) instead of the old
  `None, None` — mirroring `ninox_ui_shared::prompt_agent::deliver`'s own
  resolution exactly.
- **The prompt agent** (the default target): `ninox_ui_shared::prompt_agent
  ::ask/clear/status/reply_since/PROMPT_SESSION_ID` are real. `ask`'s own
  internal `deliver` already resolves sender identity the same way (see
  above), so this path gets the HUMAN_SENDER marker for free — no change
  needed in `app.rs` beyond the rebase onto #268.

**Separate small commit in this PR:** `ninox_core::config::AppConfig::
sessions_dir()`/`prompt_agent_workspace_dir()` were hardcoded paths under
the user's real config dir — a genuine per-machine singleton with no way to
sandbox it for a scratch-db screenshot run. Mirrors `config_path()`'s
existing `NINOX_CONFIG` env-var convention: a private `config_dir()` helper
derives from `NINOX_CONFIG`'s parent when set, and both dirs derive from it
(`sessions_dir()` keeps `NINOX_DATA_DIR` as a narrower, higher-precedence
override, preserving existing `ninox-ui-shared/src/cli.rs` call-site
precedent). One env var now sandboxes everything for both the GUI and TUI
workers.

## Design

- `PromptTarget` (`prompt_agent.rs`): `PromptAgent` (default) or
  `Orchestrator { id, name }` (picked via the `→ ninox ▾` chip,
  `pick_list`-backed).
- `resolve_delivery(target, scope, draft) -> PromptDelivery` — pure
  function: `PromptAgent` prefixes the scope (`[scope: Name] …`) when an
  island is open, `Orchestrator` never does (it's already unambiguous who
  the message goes to).
- `ReplyStripState`: `Sent { target_label, at_ms }` → `Streaming {
  target_label, reply_so_far, pane_id }` → `Error { target_label, message
  }`, plus `ContextCleared { target_label }`. `pane_id` backs "Open pane".
- `app.ninox_prompt_target`, `app.ninox_reply_strip`,
  `app.ninox_prompt_history` (+ cursor/draft-before-recall for Up/Down),
  `app.ninox_prompt_generation` (a `next_client_generation`-style counter
  gating stale replies/polls after a dismiss or newer submit).
- Up/Down history recall only fires when the ninox prompt itself is the
  focused widget — checked via `iced::advanced::widget::operate(operation
  ::focusable::find_focused())` rather than assumed, since every
  `text_input` (including this one) ignores arrow keys even while
  focused, unlike other characters, so reaching `RawKey` proves nothing
  about which field (if any) is focused.
- UI: the target chip sits in the prefix overlay after `Ask ninox`; the
  reply strip (`reply_strip.rs`) is a new row in `shell::shell`, between
  `centre` and `prompt_bar`, shown only when `ninox_reply_strip.is_some()`.

## Changes

- [x] `prompt_agent.rs`: `PromptTarget`, `resolve_delivery`,
      `ReplyStripState`, `most_recently_active_local_orchestrator`,
      `ask`/`clear` stubs shaped to match MLOPS-5415's confirmed API.
- [x] `app.rs`: new state fields; `NinoxPromptSubmitted` resolves
      delivery, pushes history, sets `Sent`, dispatches the delivery task;
      `NinoxPromptReplyReceived`/`NinoxPromptCleared` advance the strip
      (generation-gated); `NinoxPromptClear`, `NinoxPromptStripDismiss`,
      `NinoxTargetPicked`, `NinoxPromptHistoryUp`/`Down`,
      `NinoxPromptHistoryFocusChecked`.
- [x] `prompt_bar.rs`: target chip (`pick_list`) + wider `LEFT_PAD`;
      `reply_strip.rs`: the strip itself; `shell.rs`: wires it in.
- [x] `Cmd+Shift+K` → `NinoxPromptClear`; `ArrowUp`/`ArrowDown` →
      focus-checked history recall (both global, same precedence tier as
      the other prompt shortcuts).
- [x] Tests: target resolution (unscoped/scoped/picked-orchestrator),
      fallback-orchestrator selection (recency, remote exclusion, no-
      orchestrator), submit dispatches the right delivery variant with
      the right id/text, strip state machine (sent → streaming →
      dismissed; sent → error; stale generation is a no-op), clear-
      context (success/error), target picker resolution, history recall
      (Up/Down/restore-in-progress-draft/ignored-when-unfocused/typing-
      exits-recall).

## Review Focus

- `resolve_delivery` must never prefix the scope for a directly-picked
  orchestrator.
- The generation counter must actually gate `NinoxPromptReplyReceived`/
  `NinoxPromptCleared` — a stale result arriving after dismiss or a second
  submit must be a no-op, not a state resurrection (tested).
- History recall must not fire when some other field is focused — the
  `find_focused` check is the actual guard, not a view/state heuristic
  (tested with a non-prompt `widget::Id`).
- The `ToOrchestrator` arm must never deliver with an unresolved `None`
  sender — `resolve_live_caller_session_id`'s `None` result means "no
  managed ancestor, i.e. the user," and must map to `HUMAN_SENDER`, not to
  an unattested delivery.
- Manual visual check against the window per the ticket's process,
  including one real round-trip through the live prompt agent in a fully
  isolated scratch environment (scratch db/config/sessions/workspace/tmux).
