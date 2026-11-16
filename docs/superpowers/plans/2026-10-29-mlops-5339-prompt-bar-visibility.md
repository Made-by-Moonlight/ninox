# G1: GUI Prompt Bar Visibility (MLOPS-5339, round-1 try-out feedback)

**Goal:** the user's try-out feedback on the merged v1 GUI build was "the
talk to ninox prompt ... is not very obvious" — today it's one dim mono
line with a tiny "always on" label. The prompt is a primary control and
must read as a real input box at a glance. `FEEDBACK-v1-tryout-round1.md`'s
G1 item specifies: a bordered field (bg `#0b0e14`, border `#242a3a`, 8px
radius, ~36px height) with a violet `❯ Ask ninox` prefix, a brighter
placeholder, `/`/`⏎` keycap chips, and a teal focus state; `/` focuses it
globally unless another text input already has focus, Esc blurs it.

**Existing code:** `crates/ninox-gui/src/components/shell/prompt_bar.rs`
already renders the always-on prompt (wired into `shell::shell` — not
net-new, despite the design-ref brief's "complete replacement" framing
referring to the old pre-v1 shell, not this file). `tokens.rs` already
carries the v1 palette (`PANEL #12161f`, `BORDER #242a3a`, `TEAL #4fd1c5`,
etc.) — this PR restyles the existing component using those tokens plus
one addition (`PLACEHOLDER #9aa1b2`, explicitly brighter than `DIM` per the
item's own wording).

**`iced` 0.13 constraints (why the layout looks the way it does):**
- `text_input` is a leaf widget — it can't host a styled prefix label or
  suffix chips as children. The field is built as `stack![text_input,
  prefix_overlay, chips_overlay]`, with the `text_input`'s own left/right
  padding reserving room so the overlay text never sits under the typed
  value or placeholder.
- `text_input::Style` has no shadow field (unlike `container::Style`), so
  the "subtle teal outer glow" on focus is approximated with a thicker
  (2px vs 1px) teal border rather than a true box-shadow glow.
- Putting the border directly on the `text_input` (rather than a wrapping
  `container`) means the teal-on-focus state comes for free from `iced`'s
  own `text_input::Status::Focused` — no new `App`-level focus-tracking
  state needed.
- Esc-to-blur needs no handler either: `iced_widget::text_input`'s own
  `update()` already unfocuses itself and captures the event on
  `Key::Named(Escape)`.
- `/` only needs a plain global `RawKey` branch: `global_event_handler`
  already drops any key event `Status::Captured` by a focused widget, and
  a focused `text_input` captures `/` like any other character — so by the
  time our handler ever sees `/`, no text input already has focus.
- `Cmd+K` is explicitly *not* wired to this field (round-1 feedback
  follow-up): it's reserved for the G3 top-bar search.

## Changes

- [x] Add `tokens::PLACEHOLDER` (`#9aa1b2`).
- [x] Rewrite `prompt_bar::prompt_bar` as a bordered, focus-reactive field
      (`field_style`) with the `❯ Ask ninox` prefix and `/`/`⏎` keycap
      chips overlaid via `stack!`; drop the old "always on"/"scoped to
      this tree" trailing label per the item's instruction to remove it.
- [x] Add `Message::FocusNinoxPrompt`, dispatching
      `text_input::focus(prompt_input_id())`.
- [x] Wire global `/` (no modifiers, not while a terminal panel is
      capturing) to `FocusNinoxPrompt`.
- [x] Unit tests: `field_style`'s idle/focused/hovered/disabled border
      colors and widths (`prompt_bar.rs`); `/` dispatches a real focus
      command, `Cmd+K` does *not* (`app.rs`); a layout test confirming the
      field/prefix/chips are all still present as three stacked layers.

## Review Focus

- The border-color change must come from `text_input::Status`, not new
  `App` state — confirms the Esc/focus/click handling stays free from
  `iced`'s own widget focus tracking instead of drifting out of sync with
  it.
- `Cmd+K` must remain a no-op here (test:
  `cmd_k_does_not_focus_the_ninox_prompt`) — it's reserved for G3.
- Manual visual check against the window (see PR description) since this
  is a styling/layout item a unit test can only approximate.
