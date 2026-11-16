# G3: GUI top-bar search does something (MLOPS-5339, round-1 try-out feedback)

**Goal:** "This search also does nothing." The `Search sessions, plans,
brain…` field in the top nav (`components/shell/nav.rs`) is a static
`text()` label inside a styled `container` — no `text_input`, no state, no
handler. Ship real search: typing opens a grouped results dropdown
(Sessions / Plans / Brain, up to 5 hits each), `↑`/`↓`/`Enter`/`Esc`
navigate it, `⌘K` focuses the field.

## Design

- New `App` field `global_search: GlobalSearchState { draft: String,
  open: bool, cursor: Option<usize> }`. `open` tracks whether the dropdown
  is showing (true once focused, closed on Esc/selection/blur-elsewhere);
  `cursor` is the selected hit's flat index across all groups combined
  (so `↑`/`↓` move through Sessions, then Plans, then Brain as one list),
  `None` = nothing highlighted yet.
- `components/global_search.rs` (new): a real `text_input` replacing
  `nav.rs`'s static label, plus the dropdown. Pure, directly-testable
  matching/grouping:
  - `search_sessions(sessions, orchestrator_ids, query) -> Vec<SessionHit>`
    — name or `summary` substring match (case-insensitive), excludes
    orchestrator sessions and the prompt-agent singleton (same
    `worker_type::is_prompt_agent_session` exclusion PR #273 just
    centralized), capped at 5.
  - `search_brain(brain, query) -> Vec<GlobalSearchHit>` — calls
    `BrainIndex::query(query, None, QueryFilters::default())` directly
    (sync, file-backed SQLite FTS; same call `NavigateBrain` itself makes),
    capped at 5. NOT `brain_list::matches_search` over `app.brain_view
    .entries` — that cache is only populated once the Brain tab has been
    visited this session, which would make search silently miss
    everything before the user's first visit.
  - `search_plans(store, orchestrators, query) -> Vec<PlanHit>` — matches
    the *owning orchestrator's* name (no title exists anywhere to match
    against — `OrchestratorPlan`/`PlanDocState` carry none), filtered to
    orchestrators with an actually-registered plan via
    `Store::get_orchestrator_plan(id)`. Deliberately NOT the GUI's own
    `app.plan_docs` cache — that's only lazily populated once a session's
    Plan detail panel has been opened in *this* GUI session, which would
    make search miss plans that exist but haven't been viewed yet;
    `get_orchestrator_plan` reflects the store directly, same as `ninox
    plan register`/`ninox plan show` would see. One store lookup per
    orchestrator, bounded by the (typically small) orchestrator count.
  - Empty query: all three return their first 5 entries ("recent" is
    approximated as insertion/iteration order — there's no last-touched
    timestamp to sort by for any of the three sources), matching "shows
    recent items, or nothing".
- Selecting a hit:
  - Session → `Message::FleetOpenNode(id)` (switches to Fleet, opens the
    session's island — confirmed this is the one existing message that
    does both, unlike `FleetSelectIsland`/`FleetSelectNode` alone).
  - Brain → `Message::NavigateBrain` then `Message::BrainSelectEntry(id)`
    (no combined variant exists; dispatched as a `Task::batch` of both via
    a new `Message::GlobalSearchOpenHit(GlobalSearchHit)` that pattern-
    matches and issues the right one/two messages).
  - Plan → `Message::NavigateSession(orchestrator_id)` then
    `Message::SwitchDetailPanel(DetailPanel::Plan)` — `View::Plans` itself
    is an unimplemented placeholder, so a session's own Plan panel is the
    only real destination today.
- `⌘K` (global, any view) focuses the field via `text_input::focus` and
  sets `open = true` even on an empty draft (so the "recent items" list
  shows immediately) — same precedence tier as `Cmd+B`/`Cmd+F`. Field
  shows a `⌘K` keycap (reusing G1's keycap-pill pattern from
  `prompt_bar.rs`, font picked per-glyph).
- `Esc` closes the dropdown and blurs (`global_search.open = false`);
  typing with the dropdown closed reopens it (consistent with "typing
  opens a results dropdown").
- `↑`/`↓` move `cursor` through the flat hit list (clamped, no wraparound
  — matches G2's history-recall precedent of clamping at the ends rather
  than cycling); `Enter` opens the currently-cursored hit (or the first
  hit if nothing's been arrowed to yet).

## Changes

- [ ] `components/global_search.rs`: `GlobalSearchHit` enum (Session/Plan/
      Brain variants carrying what's needed to open each), `search_*`
      pure functions, the dropdown + field view.
- [ ] `app.rs`: `GlobalSearchState`; `Message::GlobalSearchChanged(String)`,
      `GlobalSearchFocus` (⌘K), `GlobalSearchClose` (Esc/blur),
      `GlobalSearchCursorUp`/`Down`, `GlobalSearchOpenHit(GlobalSearchHit)`.
      `⌘K` wired into `RawKey`'s command-modifier tier (alongside
      `Cmd+B`/`Cmd+F`) — replaces the no-op asserted by
      `cmd_k_does_not_focus_the_ninox_prompt` from G1 (test updated, not
      deleted: now asserts `⌘K` focuses global search instead).
- [ ] `nav.rs`: swap the static label for `global_search::field(app)`.
- [ ] Tests: matcher/grouping for all three sources (name/summary/tags,
      case-insensitivity, the 5-cap, prompt-agent/orchestrator exclusion
      for sessions, empty-query-shows-recent), hit-selection dispatch
      (right message(s) for each hit kind), cursor clamping, `⌘K`
      wiring, Esc-closes.

## Review Focus

- Session search must exclude the prompt-agent singleton and orchestrator
  sessions the same way G2's PR #273 centralized
  (`worker_type::is_prompt_agent_session`) — a second, independently-
  written exclusion here would risk drifting from that fix.
- `⌘K` replaces the n-op this exact key combination previously asserted in
  G1's tests — the test update must change the assertion's meaning, not
  just delete coverage.
