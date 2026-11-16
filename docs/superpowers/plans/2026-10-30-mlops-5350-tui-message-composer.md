# MLOPS-5350 item 2: TUI message composer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Message the selected session from the TUI through the same core deliver path as the GUI, and show the GUI action row's full key set in the Fleet detail pane.

**Architecture:** The `HUMAN_SENDER`-unless-a-managed-ancestor rule moves into `ninox_core::messaging::resolve_interactive_sender` (one commit), replacing three inlined copies; the GUI's own detail-pane composer, which sent unattested `from: None`, uses it too. The TUI gets a `Modal::Message` one-line composer opened by `t`, submitting `Action::SendMessage`, which `perform` runs through `run_op` with `deliver_message_with_acp` and `MessageKind::Freeform`.

**Tech Stack:** Rust, ratatui (`ninox-tui`), `ninox-core::messaging`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md`, Worker A item 2.

## Global Constraints

- Key is `t`, not the brief's `m`: `m` stays Fleet's density toggle (agreed in the item read-back). `ctrl+t` keeps cycling the prompt target.
- Restart (item 3) is not in this PR's action row; it lands with its own key in item 3.

## Review Focus

1. Command letters typed into the composer (`q`, `x`, `t`, `R`) are text, never commands — the modal owns every key while open.
2. A blank or whitespace-only draft never sends.
3. A long draft keeps its caret end visible (scrolls left by display width) rather than clipping or wrapping.
4. The composer is addressed by id at open time, so a selection change behind the modal can't redirect it.

---

### Task 1: Share the sender resolution in core

- [x] **Step 1:** Add `messaging::sender_or_human(Option<String>) -> String` and `async messaging::resolve_interactive_sender(&Store) -> (String, Option<runtime::PaneIdentity>)`; move the TUI's `prompt_sender` tests to core as `sender_or_human_*`.
- [x] **Step 2:** Replace the inlined copies in `ninox-ui-shared/src/prompt_agent.rs::deliver`, the GUI's `NinoxPromptSubmitted` and the TUI's `Action::SubmitPrompt`; point the GUI's `FleetSendMessageSubmit` at it instead of `from: None`.
- [x] **Step 3:** `cargo clippy --workspace --all-targets -- -D warnings`; commit on its own.

### Task 2: TUI composer

- [x] **Step 1:** Tests in `state.rs`: `t_opens_a_message_composer_for_the_selected_session_and_enter_sends_it`, `the_message_composer_takes_command_letters_as_text_and_esc_cancels`, `t_with_nothing_selected_says_why_instead_of_opening`, `prefix_t_opens_the_composer_from_inside_an_attached_pane`.
- [x] **Step 2:** `MessageComposer { id, name, draft }`, `Modal::Message`, `Action::SendMessage { id, message }`; `t` in `command()` (so bare on the list, prefix+t from a pane); modal keys Esc/Enter/Backspace/Char; help row.
- [x] **Step 3:** Tests in `view.rs`: `fleet_detail_panel_lists_the_message_key_with_the_other_actions`, `message_composer_names_its_target_and_keeps_the_caret_end_of_a_long_draft_in_view`, `tail_fit_keeps_the_end_that_fits_by_display_width`; add `Message` to the modal contrast test.
- [x] **Step 4:** Draw the modal (`tail_fit` for the input line) and add `t message` to the detail pane's key row.
- [x] **Step 5:** `perform`'s `Action::SendMessage` arm via `run_op`.
- [x] **Step 6:** `cargo test --workspace`, clippy; commit.
