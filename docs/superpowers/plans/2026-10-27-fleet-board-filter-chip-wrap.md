# Fleet Board Orchestrator Filter-Chip Wrap Fix (MLOPS-5337)

**Goal:** On the v1 `ninox-gui` fleet board, with many orchestrators (~18 in
the real data dir) the orchestrator filter-chip row squeezes every chip
after the first to roughly one character wide, wrapping each name
letter-by-letter and inflating the row to hundreds of pixels tall, pushing
the Kanban columns down. Chips must keep their natural label width and
wrap onto further rows instead of ever shrinking below their label.

**Root cause:** `crates/ninox-gui/src/components/fleet_board.rs` laid the
chips out with a plain, non-wrapping `iced::widget::row(chips)`. `iced`'s
flex layout for `Row` (`iced_core::layout::flex::resolve`) hands each
`Shrink`-sized child a max width drawn from a single `available` budget
that only ever decreases across the row and is never reset or checked
against zero before being handed to the next child. Once the combined
natural width of ~18 chips exceeds the container width, the budget runs
out partway through — later chips get a near-zero/negative width limit,
forcing their text to word-wrap one character at a time. This is
order-dependent exhaustion of a shared budget, not proportional
(CSS-flexbox-style) shrinking, and it has no line-wrap fallback.

**Fix:** `iced` 0.13.4 (already pinned in `Cargo.lock`) ships a built-in
`Row::wrap()` → `Wrapping` widget (`iced_widget::row::Wrapping`) that lays
out every child at its true unconstrained natural width and only starts a
new row when a child doesn't fit — it never shrinks a child below that
natural size. Switching the chip row to `.wrap()` is a complete, minimal
fix; no custom widget or heuristic width estimation is needed.

**Tech Stack:** Rust, `iced` 0.13 (`ninox-gui`), `cargo test -p ninox-gui`.

## Changes

- [x] Change `tab_chip` to take `&ColorScheme` instead of `&App` (it only
      ever used `app.scheme`), so the chip-row construction can be unit
      tested without a full `App`/`Engine`/`Store`.
- [x] Extract the orchestrator chip-row construction (previously inlined
      in `fleet_board()`) into `orchestrator_chip_row(&ColorScheme,
      &[Orchestrator], Option<&OrchestratorId>) -> Element<Message>`, and
      apply `.wrap()` to the row it builds.
- [x] Add a dev-dependency on `iced_tiny_skia` (already resolved
      transitively via `iced`'s default `tiny-skia` fallback-renderer
      feature — see `Cargo.lock`) to get a headless, software-only
      `iced_core::Renderer` for layout tests, with no GPU required.
- [x] Add a layout test (`orchestrator_chip_row_never_shrinks_chips_below_their_natural_width`)
      that builds an 18-orchestrator chip row (mirroring the real
      fleet-board data dir that triggered this bug), lays it out via the
      real `Widget::layout()` call at a width that forces wrapping, and
      asserts: (a) every chip's measured width matches its unconstrained
      natural width, and (b) chips land on more than one row. Verified
      red (fails with the old, un-wrapped row — reproducing the reported
      squeeze) before green (passes with `.wrap()` applied).

## Review Focus

- The test must fail against the pre-fix code (confirmed manually during
  development) — it needs to actually exercise the production
  `orchestrator_chip_row` function, not just assert on `iced`'s own
  `Row::wrap()` behavior in isolation.
- No chip may ever be given a width narrower than its own label's natural
  (unconstrained) width, for any number of orchestrators.
- Manual GUI verification against the real `~/Library/Application Support/ninox`
  data dir (not just the unit test) is required per the ticket, since this
  is a layout/rendering bug that a pure unit test can only approximate.
