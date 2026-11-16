# MLOPS-5339 GUI parity, item 3: Help overlay

Spec: `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5339-gui-parity.md` item 3. There's no board;
the overlay follows the existing v1 modal look (`remotes::add_remote_modal`).

## Tasks

1. **`components/help_overlay.rs`**: `SECTIONS`, the single list of shortcuts, grouped by where
   they apply (`Scope`: Anywhere, Fleet, Brain, BrainGraph, Spawn). Each `Shortcut` holds the
   `Chord`s it's typed with, and the chips are rendered from those chords. Chord labels use ⌘/⇧ on
   macOS and Ctrl+/Shift+ elsewhere. A bare letter stays lowercase. Chips containing a symbol render
   in `GLYPH_ALT` (Menlo), because Spline Sans Mono has no ⌘, ⇧ or arrow glyphs. The decided map
   from the brief is listed first, worded as decided.
2. **Wiring**: `App.help_open` and `Message::ToggleHelp`. `?` in the plain-key block opens it. While
   it's open the overlay takes every key, and Esc or `?` closes it, as does a backdrop click. It
   stacks above every other modal. `[` is bound to `ToggleTreeCollapsed` (in the decided map, but
   unbound before this).
3. **Drift guard**: `every_listed_shortcut_does_something` replays every chord through the real
   `RawKey` handler in setups for its scope. Each chord must return a task or change visible state,
   so a dead key can't be listed. It caught `⌘B`, which flipped `sidebar_hidden`, a field no v1
   view reads. `⌘B` now toggles the tree rail like `[`, and `sidebar_hidden` is deleted from the
   GUI and from `AppConfig`; old config files that still have the key load fine. Wiring the field
   to the rail instead was rejected because its persisted default (`false`) would start everyone
   with the tree expanded.
4. **Reflow on tree toggle**: `ToggleTreeCollapsed` now runs `resize_terminals`. The rail's width
   feeds the PTY size, and `[`/`⌘B` made this toggle a single keypress.
5. One isolated window capture.
