# G4: Spawn dialog v1 restyle (MLOPS-5339, round-1 try-out feedback)

**Goal:** "the Spawn view doesn't match the new style." `spawn_modal.rs` is
still the old warm "Field Notes" journal-entry theme (`ColorScheme`, serif
title split across two fonts, hard offset shadows, a segmented Entry-type
toggle, pill chips in the old palette) — none of it matches the flat dark
`tokens::` system every other v1 surface uses. Approved design:
`Spawn.dc.html`.

## Design

Tokens, straight from `Spawn.dc.html`'s `.ninox` CSS vars (same values
`components::shell::tokens` already defines): bg `#0b0e14`, panel
`#12161f`, panel2 `#171c28`, border `#242a3a`, text `#e8e9ed`, dim
`#8b92a5`, teal `#4fd1c5`. Font: sans only (`style::SANS`/`SANS_BOLD`) —
no `SERIF`/`SERIF_ITALIC` anywhere in this file after this PR.

- Dialog: 500px wide, `tokens::PANEL` background, `1px #2f3750` border,
  12px radius, over a `rgba(5,7,11,.66)` backdrop (not `s.shadow`-based).
- Header: "Spawn a session" (plain, `SANS_BOLD`), an `esc` keycap on the
  right, bottom border in `tokens::BORDER`.
- **Kind**: two description cards (`role="radiogroup"` in the mock, a
  plain row of two buttons here), replacing the old segmented toggle.
  Orchestrator — "Coordinates workers; gets its own island in Fleet."
  Standalone — "One agent in a workspace you pick, no workers." Selected:
  teal border `#2a7d75` + tinted background `#122826`. Standalone reveals
  the Workspace field, same as today.
- **Name**: a real bordered input (teal focus ring, approximated the same
  way `prompt_bar`/`global_search` already do — no shadow field on
  `text_input::Style`, so a thicker teal border stands in for the glow)
  plus a dim hint line: "Shown on its island. Letters, digits and dashes."
- **Agent**: pills, one per `registry.enabled_names()` — already correct
  logic (G1/G2 already established "only existing harnesses" is the
  existing behavior here), restyled only.
- **Model**: `pick_list`, restyled only — `effective_model`/discovery
  logic unchanged.
- **Brain catalogue**: unchanged logic
  (`AppConfig::catalogue_options()`) — restyled only. No "All sources"
  entry: Brain sources haven't landed on `v1` yet (confirmed — no
  `BrainSource`/merged-catalogue code exists), so that part of the spec
  is unmet and out of scope here, per "pick the most spec-consistent
  option and note it."
- **Type** (new field, MLOPS-5416 has now landed — #265/#270 merged):
  `pick_list` of `state.config.worker_type_registry().user_facing_names()`
  (already excludes `hidden` types like the prompt agent), defaulting to
  `"Implementor"`. Recorded on the spawned `Session.worker_type` (both the
  Standalone and Orchestrator confirm paths currently hardcode `None`).
  **Scope decision:** this dialog's spawn path is an *interactive* GUI
  spawn (`spawn_util::spawn_interactive_session`), not `ninox spawn`/
  `run_spawn` (that CLI command is for an orchestrator spawning a worker,
  per this file's own doc comment — "never from here"). A worker type's
  other effects there (composing a role-brief into a task prompt,
  resolving success criteria) don't map onto an interactive spawn with no
  task prompt. Only `Session.worker_type` itself is set here, so the
  choice is recorded/queryable (Worker Types tab, `ninox worker-status`)
  — permission-profile seeding (the one other type effect that *would*
  mechanically apply, mirroring `ninox_ui_shared::prompt_agent`'s own
  `.claude/settings.json` merge for its type) is a genuine follow-up, not
  built here, and called out in the PR.
- **Footer**: cost estimate (`estimate_text`, unchanged pure function —
  only its rendering moves to v1 tokens), Cancel, primary teal "Spawn ⌘⏎"
  button (`tokens::TEAL`-family, matching `nav.rs`'s "+ Spawn" button as
  the established primary-CTA precedent rather than hand-deriving the
  mock's literal `#123f3b`/`#bff3ec`).
- `Esc` cancels (already wired); `⌘⏎` submits — **not currently wired
  anywhere** (only Enter-while-the-Name-field-is-focused submits today).
  New global `RawKey` branch, same precedence tier as the existing Esc
  check.

## Changes

- [ ] `spawn_modal.rs`: full rewrite to `tokens::`-based styling; `SpawnForm`
      gains `worker_type: String` (default `"Implementor"`).
- [ ] `app.rs`: `Message::SpawnFormWorkerType(String)` + handler (mirrors
      `SpawnFormCatalogue`'s plain-setter shape); `SpawnFormConfirm`'s two
      `Session` constructions set `worker_type: Some(form.worker_type)`
      instead of `None`; `⌘⏎` wired globally while `spawn_modal.is_some()`.
- [ ] Tests: default `worker_type`, the new message handler, `⌘⏎`
      dispatches `SpawnFormConfirm`, confirm records the chosen
      `worker_type` on the spawned `Session` (both kinds), the Type
      picker's options never include a hidden type (regression guard
      against a future swap from `user_facing_names()` to `names()`).

## Review Focus

- No `SERIF`/`SERIF_ITALIC`/`ColorScheme` (`s.*`)/hard-shadow usage must
  remain in this file — the explicit regression the feedback names.
- The Type field's option source must stay `user_facing_names()`, never
  `names()` — a hidden type (the prompt agent) must never be selectable.
- `⌘⏎` must only submit while the spawn modal is actually open, same
  precedence tier as the existing Esc-cancels check, and must not steal
  the key from the Name field's own plain-Enter submit.
