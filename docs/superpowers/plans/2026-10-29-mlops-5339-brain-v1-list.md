# MLOPS-5339 follow-up: GUI Brain tab, PR1 — v1 List view

Spec: `Brain.dc.html` / `BrainGraph.dc.html` / `BrainSources.dc.html` under
`.claude/design-ref/fleet-overview-v1/` (parent checkout), brief
`BRIEF-MLOPS-5339-brain.md`. This plan covers PR1 only (List); Graph and
Sources land as PR2/PR3, stacked on top, per the same brief.

## Scope (PR1)

Replace the Field-Notes-themed `brain_panel`/`brain_pinboard` with the v1
dark-token List view, and turn Graph/Sources into v1-styled placeholders
(no leftover warm theme anywhere in the Brain tab — brief requirement 5).

### State (`ninox-gui/src/app.rs`)

- `BrainMode`: rename `Catalogue` -> `List`, `Pinboard` -> `Graph`, add
  `Sources`. Default -> `List`.
- `BrainViewState`: drop `open_drawers`, `layout` (`ForceLayout`),
  `dragging` — the old pinboard's drag/physics model doesn't carry over to
  the new deterministic, non-drag Graph interaction model being designed
  for PR2. Add `kind_filter: Option<String>`, `repo_filter: Option<String>`,
  `tag_filter: Option<String>`, `sort: BrainSort`,
  `last_indexed: Option<Instant>`, `outlinks: Vec<BrainEntry>` (the
  reading-pane footer's "Links to", via the existing but previously-unused-
  by-GUI `BrainIndex::outlinks`).
- Messages: drop `BrainPhysicsTick`/`BrainDragStart`/`BrainDragMove`/
  `BrainDragEnd`/`BrainToggleDrawer`; add `BrainSetKindFilter`,
  `BrainSetRepoFilter`, `BrainSetTagFilter`, `BrainSetSort`.
- `wants_physics_tick` + its subscription wiring: removed.
- `brain_pinboard.rs` + `force_layout.rs`: deleted (dead once Graph is a
  placeholder; PR2 builds a new renderer from scratch per spec — a
  neighbourhood/clusters/everything scope model has nothing in common with
  continuous force integration).

### New module: `ninox-gui/src/components/brain_list.rs`

Pure, unit-tested kind normalization (brief: "Merge singular and plural
kinds... Normalise in one place, and unit-test it") plus the List view's
rendering:

- `kind_key`/`kind_label`/`kind_color`: canonical kind id, display label,
  v1 token color. Fixes the decision/decisions duplicate-row bug.
- `kinds_sorted_by_count`: kinds column ordering.
- `visible_entries`: kind + repo + tag + search filtering, three sorts
  (Recent/A-Z/Most-linked, the last via `brain_view.edges` degree).
- `toolbar`, `kinds_column`, `entry_list`, `reading_pane`, `list_body`.

`brain_panel.rs` keeps the pure wikilink helpers (still used by the reading
pane and `BrainLinkClicked`) and becomes the mode dispatcher: `List` ->
`brain_list::list_body`, `Graph`/`Sources` -> `shell::placeholder(...)`.

### Tokens (`shell/tokens.rs`)

Add `BLUE` (`#9db4ff`, Repos), `PINK` (`#f472b6`, Concepts), `CYAN`
(`#67e8f9`, Symbols), `ORANGE` (`#fb923c`, Hazards) — pulled from
`Brain.dc.html`'s own CSS, matching the existing token provenance.

## Out of scope for this PR

Graph rendering, Sources CRUD, merged "all sources" index, TUI, sync engine
semantics, the ninox prompt, ⌘F global focus shortcut (search still works,
just not keyboard-focused via a global binding — noted as a deviation).

## Test plan

- `cargo test -p ninox-gui`: updated/removed pinboard-physics tests, new
  `brain_list` unit tests (kind normalization, sort, filtering).
- `cargo clippy --workspace --all-targets -- -D warnings`.
- Manual: launch against a copy of the real brain data dir (never the live
  store — `NINOX_DATA_DIR` pointed at a copy), window-only screenshot of
  List with Decisions selected and an entry open, compared against
  `Brain.dc.html`.
