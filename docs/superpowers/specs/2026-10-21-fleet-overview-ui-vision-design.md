# Fleet Overview UI/UX Vision (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Captures a
proposed direction so an implementation plan can be cut from it later.
Lives on the long-lived `v1` branch, kept separate from `main` so it can
be installed and tested without affecting the current release.

**Implementation target:** this repository (`ninox-core`, `ninox-app`).
File and line references below reflect the code as of this writing;
treat them as orientation, not guaranteed locations.

**Design reference:** the canonical visual spec is the design artifact
(27 Oct 2026 revision); artboard sources are mirrored in
[`assets/fleet-overview-v1/`](assets/fleet-overview-v1/) — `canvas.json`
holds the board titles and the `scaleflow` note that the "Fleet at
scale" and "Opening an island" sections below implement. Where this
document and the artboards disagree, the artboards win.

## Context

This is the first of several specs for a larger next-gen refactor
(separate specs will cover the auto-planning/sectioning algorithm, the
hexagonal/module-boundary refactor of `ninox-core`, and the expanded CLI
surface). This one defines the shared UI/UX vision everything else
renders into, for both the TUI and the GUI.

Three sibling specs already exist, merged but unimplemented, and are
relevant prerequisites rather than things this spec re-designs:

- `docs/superpowers/specs/2026-10-05-session-self-identification-design.md`
  — `ninox whoami` + hook-based role reminders. Ready to implement as-is.
- `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` — SSH
  `machine add`, ControlMaster/ControlPersist transport already partially
  built in `crates/ninox-core/src/remote.rs` (329 lines). 10 open
  questions remain there (exec-based attach over SSH, `SessionBackend`
  trait shape, PID liveness, remote git diff, tmux-vs-ptyd) — this spec
  only adds how a remote-hosted node is *shown*, not how remoting works.
- `docs/superpowers/specs/2026-10-05-worker-permission-model-design.md`
  — scoped allow-list + merge-gate hook for autonomous workers. Relevant
  to worker types below (a worker type's "tool/permission profile" is
  this spec's name for configuring that model per type).

Current state this spec changes:

- **No worker-type/role concept exists.** `Session.agent_type` in
  `crates/ninox-core/src/types.rs` holds the *harness* name
  (`"claude-code"`, `"codex"`, …), not a role. There is no `WorkerType`
  enum or table anywhere. Every worker is implicitly "implementation."
- **The graph view is TUI-only.** `crates/ninox-app/src/tui/graph.rs`,
  built on the `rataflow` crate per
  `docs/superpowers/specs/2026-10-19-rataflow-fleet-graph-design.md`,
  already renders live message-flash edges between sessions (recent
  bugfix branches `graph-view-live-comms` / `graph-view-status-polish`
  are on this code). That spec explicitly scopes the graph to TUI only.
  The GUI (`ninox-app`, Iced) has no equivalent: `force_layout.rs` is
  brain-pinboard-only, `fleet_board.rs` is a plain Kanban board.
- **No cross-harness messaging.** `crates/ninox-core/src/harness.rs` is
  a data-driven registry of spawn-time CLI args per harness only.
  `crates/ninox-core/src/plugin.rs` is a one-line stub. `messaging.rs`
  (518 lines) only delivers between same-harness sessions via the
  session-socket/inbox mechanism.
- **No nested orchestration.** No spec or code reference for an
  orchestrator spawning another orchestrator exists today.
- Brain external-store support (S3) already ships
  (`crates/ninox-core/src/brain_sync/{s3.rs,manifest.rs,mod.rs}`,
  `docs/BRAIN.md`); this spec does not touch brain storage.

## v1 replaces the shell

v1 is a **complete replacement** of the existing GUI and TUI shells, not
an additional section or tab layered next to them. A prior attempt added
a "V. Fleet overview" sidebar entry next to the existing cream sidebar
and drew edges directly between orchestrators; that attempt was rejected
(27 Oct 2026) on both counts — see "Two edge types" and "Fleet at scale"
below for why orchestrator-to-orchestrator edges are wrong.

Removed:
- The existing GUI sidebar sections (including the standalone "Workers"
  list) and the TUI's separate Graph tab — superseded by the Fleet tab's
  tree + island canvas, with identical core logic behind both renderers.
- The standalone Pull Requests list — PR state now shows on each node,
  and PR review and Marginalia comments move into the detail panel (see
  "Opening an island" and "Node content and appearance" below).

Kept, restyled to this design:
- The Kanban board (Working / PR Open / CI Failed / Review / Mergeable /
  Done) — becomes its own "Board" nav tab (see "Navigation" below),
  unchanged in function.

## Goals

- One core (`ninox-overview`, new crate) drives the graph/tree/state
  logic for both the TUI (ratatui) and GUI (Iced) renderers, so the two
  install paths (`ninox-gui`, `ninox-tui`/`nx` — packaging only, not
  designed here) stay behaviorally identical.
- Tracing any agent's position and activity stays possible regardless of
  how deep orchestrator nesting goes, how many agents are running, or
  how many unrelated orchestrator trees exist at once.
- Defining a new worker type is as easy as filling in a form or asking
  the ninox prompt in plain language.

## Non-goals

- The auto-planning/sectioning *algorithm* — this spec only defines how
  a generated plan is reviewed and approved, not how it's generated.
- The hexagonal refactor of `ninox-core`'s ~25 modules — separate spec.
- CLI command surface expansion — separate spec.
- Remote-session transport mechanics — covered by the existing
  remote-sessions spec; this one only adds the on-graph badge.

## Design

### Navigation

Six top-level tabs, same order and number-key binding in both surfaces:

1. **Fleet** — the island/tree view this spec defines (default tab).
   Carries a count badge for sessions that need the user (CI failed,
   blocked, or waiting on an answer).
2. **Board** — the existing Kanban, restyled (see "v1 replaces the
   shell" above).
3. **Plans** — plan review across the fleet (see "Plan review" below).
4. **Brain** — unchanged.
5. **Worker Types** — the worker-type management screen (see "Worker
   types" below).
6. **Remotes** — remote-machine management (see the remote-sessions
   spec).

### Screen skeleton (TUI and GUI identical)

Nav bar plus four fixed regions below it, same in both surfaces:

```
┌─────────────────────────────────────────────────────────┐
│  Fleet · Board · Plans · Brain · Worker Types · Remotes   │
├─────────┬───────────────────────────────┬─────────────┤
│  Tree    │                                │   Detail    │
│(collaps- │        Graph canvas            │   panel     │
│ ible,    │                                 │ (appears on │
│ starts   │                                 │  selection) │
│collapsed ├───────────────────────────────┤             │
│ on Fleet)│   ninox prompt (always on)     │             │
├─────────┴───────────────────────────────┴─────────────┤
│  Context-sensitive controls (full width, bottom)        │
└──────────────────────────────────────────────────────────┘
```

See [`Legend.dc.html`](assets/fleet-overview-v1/Legend.dc.html) for the
canonical skeleton diagram, status colors, edge types and node styles
referenced throughout this section and the next two; small-fleet
reference for the layout below is
[`Main.dc.html`](assets/fleet-overview-v1/Main.dc.html) (GUI) /
[`TUI.dc.html`](assets/fleet-overview-v1/TUI.dc.html) (TUI).

- **Tree** (left): an expandable text tree of every orchestrator →
  sub-orchestrator → worker relationship, the primary tool for tracing
  depth regardless of graph zoom level. Does not shrink or share space
  with the detail panel. **Collapsible** (`[` key, or the rail button):
  collapsed, it's a 44px rail showing a toggle button and a vertical
  "Sessions · N trees" label; expanded, it's a ~238px panel grouped by
  machine (`This machine`, then one group per remote machine with its
  connection state). **Starts collapsed on the Fleet tab** — the
  islands there already show every orchestrator, so the tree would be
  redundant by default; it stays available to toggle open at any time.
- **Graph canvas** (center): the node-graph, see below.
- **Detail panel** (right): appears when a node is selected. Does not
  overlay or replace the tree.
- **ninox prompt** (bottom of the graph column, always visible, never a
  toggled overlay): a persistent natural-language input. See "ninox
  prompt" section below.
- **Context-sensitive controls** (bottom, full width): the available
  actions for whatever is currently selected, computed once in the core
  crate from the node's type/state. The TUI renders this as the existing
  footer-hint convention (`draw_footer()` in `tui/view.rs` today — single
  line, mode pill + key hints that change with focus); the GUI renders
  the identical action list as a button row. Selecting a plain worker
  instead of an orchestrator drops "view sub-tree" from both
  automatically — one list, two renderings.

### Node content and appearance

See [`GUI-Inspect.dc.html`](assets/fleet-overview-v1/GUI-Inspect.dc.html) /
[`TUI-Inspect.dc.html`](assets/fleet-overview-v1/TUI-Inspect.dc.html) for
the full node-inspection content referenced below.

Every node (worker or orchestrator) carries:

- Worker-type badge (built-in or custom — see "Worker types" below)
- Harness badge (claude-code / codex / aider / …)
- Live status (working / blocked / idle / done)
- Brief (the task it was spawned with)
- Position: breadcrumb to root, depth indicator (e.g. "level 2 of 3")
- Live activity (current/last tool call, e.g. "⚒ bash ×3")
- Artifacts (PR/branch/worktree, relevant brain entries). There is no
  separate Pull Requests list — PR state shows here as a chip (`PR open
  #208`, `mergeable`, `CI failed`, …), and opening the node's detail
  panel is where PR review and Marginalia comments live (see "Opening
  an island" below).
- If remote-hosted: a connection-status badge (green=connected,
  amber=reconnecting, red=unreachable) next to the harness badge — no
  separate "Machines" navigational concept; remote nodes sit in the
  same tree/graph as local ones.

**Default node style is a full card**: worker type, harness badge, name,
a status-color bar, and the live tool-call line underneath. A
**compact/minimal toggle** switches every node to a small status+harness
dot with just the name, for navigating very large trees without the
full card's footprint. Both styles are views over the same node data;
nothing is lost switching between them except on-canvas detail (the
detail panel still has everything).

Orchestrator (group) nodes additionally show expand/collapse state and a
child-count badge. A node that has hit the depth cap and is reporting
upward instead of spawning further is marked with a red ring,
independent of card/compact mode.

### Two edge types

The graph is **not a pure tree** — it's a tree (hierarchy) plus a mesh
(communication), and both are first-class:

- **Hierarchy edges**: solid, thin, fixed — define who-spawned-whom and
  the graph's layout. Stable; don't animate on their own.
- **Communication edges**: drawn for a message between any two agents,
  regardless of tree position (cross-harness messages — a claude
  orchestrator messaging a codex worker, no distinct visual style from
  same-harness, the two different harness badges already tell that
  story — messages between nodes in completely unrelated orchestrator
  trees, and a depth-capped worker "talking back up" to a non-parent
  ancestor). They render differently depending on whether a hierarchy
  edge already exists between the pair:
  - **Parent ↔ direct child**: no new edge is drawn. This reuses the
    existing live-flash mechanism already shipping in `tui/graph.rs`
    (`graph-view-live-comms` branch) — a brief pulse/glow on the
    existing hierarchy nesting boundary, fading after ~1s, no badge, no
    persistent log entry. This is what already ships today for routine,
    high-frequency parent-child traffic; this spec leaves it unchanged.
  - **Any pair with no existing hierarchy edge** (cross-tree, a
    broadcast, a depth-capped worker reporting to a non-parent
    ancestor): this is the new behavior this spec introduces. With no
    existing boundary to flash, a transient dashed edge is drawn
    between the two nodes specifically for that message, then fades. In
    a dense view these may be numbered and logged in a small
    message-log list — logging is reserved for this novel/rare case
    rather than applied to routine parent-child chatter, which would
    otherwise flood the log.

**Invariant:** a ghost (proposed, not-yet-running) node has no running
agent behind it and can therefore never be a communication-edge
endpoint, under either case above. Any message "about" a pending plan is
addressed to the real orchestrator that owns the plan, never to the
ghost nodes themselves.

**No third, standing edge type.** These are the only two edge kinds the
graph canvas draws — there is no persistent/non-fading edge kind, even
for a relationship that is itself persistent. The pre-existing
`worker-status` "declared dependency" feature (`SessionDep`, plus
PR-branch-stacking) names a real, standing relationship that can legitimately
cross orchestrator trees, but it is not an edge type in this model: drawing
it as a standing dotted line (as the code did before this clause existed)
reads as a hierarchy/communication edge while fading like neither, and
breaks the "isolated per-orchestrator environments" expectation below at
the Fleet tab's overview. It renders instead as a small badge on the dependent
node (count of live dependencies), leaving the canvas's only edges as the
two kinds above.

At every zoom level — including the Fleet tab's overview of all
orchestrator islands — both edge types render inline. Proposed-but-unapproved plan sections
(ghost nodes, below) also render inline at every zoom level rather than
collapsing behind a summary badge, accepting the extra visual density in
exchange for a pending plan's shape being visible without drilling in.

### Fleet at scale

The Fleet tab's view once more than a handful of orchestrators are
running: one **island** per root orchestrator tree (an orchestrator plus
every sub-orchestrator and worker beneath it), plus one island for
unparented sessions. This is the same tree/graph data as "Nested
orchestration" below, rendered as islands instead of one shared canvas.

- **Hierarchy only inside an island.** An island draws its own
  orchestrator-on-top, workers-on-a-rail hierarchy. **No edge ever joins
  two orchestrators** — a hard invariant, not a zoom-dependent
  simplification. It applies at every density: a communication edge
  between unrelated trees still renders as the transient dashed edge
  described in "Two edge types" above, never as a persistent hierarchy
  line between islands.
- **Layout:** islands pack into columns. **Vertical scroll only — never
  horizontal.**
- **Sort:** a toolbar control with three orders — needs-you first (CI
  failed, blocked, or waiting on the user), most active, and A–Z.
- **Folding:** within an island, workers in a finished state (done or
  stopped) collapse into one `▸ N done · M stopped` row rather than
  listing each individually; clicking it unfolds them.
- **Remote trees:** an island rooted on a remote machine carries a
  `REMOTE · <machine>` band with the same connection-status dot used
  elsewhere (green connected / amber reconnecting / red unreachable).
  The session tree groups orchestrators under their machine the same
  way, and the toolbar has a Machine filter (All / Local / `<machine>`…)
  alongside Sort.
- **Density toggle:** the existing full-card / compact toggle (see
  "Node content and appearance" above) applies per-island here too.
- **Header:** the tab shows a running total (`N orchestrator trees · M
  workers`), the needs-you count, and how many sessions are folded away.

See [`FleetAtScale.dc.html`](assets/fleet-overview-v1/FleetAtScale.dc.html)
(GUI) and [`TUIAtScale.dc.html`](assets/fleet-overview-v1/TUIAtScale.dc.html)
(TUI), both drawn against the real 27 Oct 2026 fleet (18 orchestrators
plus one example remote machine), and the `scaleflow` note in
[`canvas.json`](assets/fleet-overview-v1/canvas.json).

### Opening an island

Clicking (or pressing Enter on) an island, or clicking an orchestrator in
the tree, opens it:

- **Shows** only that orchestrator's tree, in full cards, with hierarchy
  edges, sub-orchestrators, and any ghost plan nodes (see "Plan review"
  below). Finished workers still fold into one stacked card, same as the
  island view.
- **Breadcrumb:** `Fleet › <orchestrator>`, shown in place of the tab's
  default header. **Esc** (or clicking the breadcrumb) returns to the
  Fleet tab with the island still selected, not an unselected fleet
  view.
- **Clicking a worker in the tree** opens its orchestrator with that
  worker pre-selected in the detail panel, rather than requiring two
  separate clicks.
- **Context-sensitive:** the detail panel and bottom action row follow
  the current selection exactly as on the Fleet tab — selecting a
  worker here is how PR review and Marginalia comments are reached (no
  separate Pull Requests list; see "v1 replaces the shell" above).
- The ninox prompt remains docked underneath, scoped to the open tree
  (its placeholder reflects this, e.g. "ask ninox about `<orchestrator>`
  — …") rather than the whole fleet.

See [`FleetFocused.dc.html`](assets/fleet-overview-v1/FleetFocused.dc.html)
— Ninox's tree opened from the state in `FleetAtScale.dc.html`.

### Nested orchestration and the depth cap

An orchestrator can spawn a sub-orchestrator and hand it a whole section
of work to own; that sub-orchestrator can spawn further sub-orchestrators
or workers. This is real delegation, not just a visualization — a
sub-orchestrator manages its own worker pool semi-autonomously once
spawned.

Depth is **hard-capped, default 3, configurable**. Once a chain would
exceed the cap, the would-be next layer is not allowed to be another
orchestrator: the spawning orchestrator instead spawns a plain worker
whose brief includes instructions to report back up the tree via a
message (a communication edge) rather than spawning further. This keeps
tracing bounded — the tree sidebar's breadcrumb is always at most
depth-cap levels deep — without hard-blocking work that would otherwise
need a fourth layer.

### The ninox prompt

A persistent natural-language input docked under the graph in both
surfaces. It is a secondary way to query or act on the fleet, not a
replacement for the graph/tree/detail panel — the graph stays the visual
source of truth.

Supported today's-design scope:

- **Read queries**: "what's blocking the webhook rewrite?", "show me
  everything claude-code is doing" — answers render either inline in the
  prompt area or by highlighting/selecting the relevant node(s) in the
  graph, not as a separate chat transcript.
- **Action requests**: "spawn an orchestrator for the billing
  migration." Gate behavior:
  - A request that resolves to a single worker or single orchestrator
    with a clear brief executes immediately.
  - A request that implies multiple sections or dependencies (an epic)
    always produces a **plan for review** (ghost nodes) rather than
    executing directly.
- **Worker-type drafting**: "make a Tester type that only runs tests and
  reports, no fixing" opens the worker-type editor pre-filled for review
  (see below) rather than silently creating one.

### Plan review (ghost nodes)

Triggered by a prompt request that implies multiple sections, or by an
orchestrator's own auto-planning (algorithm out of scope here). Renders
in the *same* skeleton and *same* graph, with proposed nodes drawn as
ghosts rather than a separate screen:

- Ghost nodes: dashed border, diagonal hatch fill, distinct in both full
  card and compact mode — unmistakably "not running yet."
- The tree shows proposed nodes with a hollow marker instead of the
  normal status dot.
- Selecting a ghost node's detail panel lets you edit its brief, worker
  type, and dependencies before anything runs.
- The ninox prompt remains active during plan review — natural-language
  edits to the plan ("swap backfill-job to the Researcher type first")
  are a valid way to revise it.
- Bottom controls: **Approve & spawn all**, **Approve selected only**,
  **Add section**, **Discard plan**.
- On the Fleet tab, an orchestrator island with a pending plan shows its
  ghost children inline (per the density decision above), not collapsed
  behind a badge.

### Worker types

See [`WorkerTypes.dc.html`](assets/fleet-overview-v1/WorkerTypes.dc.html)
for the management screen referenced below (the "Worker Types" nav tab).

A worker type is a named, reusable config: brief/prompt template,
tool/permission profile (ties into the worker-permission-model spec's
scoped allow-list), and default harness. **User-definable and
extensible** — ninox ships a few built-ins (Implementation, Researcher,
Evaluator) as examples, not a closed set.

Management screen: a list (built-in types and custom types, visually
distinguished) on the left, a form editor on the right (name, brief
textarea, default-harness field, toggleable permission chips). Two
creation paths converge on the same editor:

1. Directly: "+ New type" opens a blank form.
2. Via the ninox prompt: a natural-language request drafts a type and
   opens it pre-filled in the same editor for review/edit before saving
   — never saved silently.

**Export/import**: a worker type can be exported (to a shareable
file/snippet) and imported from one, so a well-designed custom type can
be shared between users or projects without rebuilding it by hand.
Format is out of scope for this spec (likely TOML, matching the
project's existing config-file convention in `config.toml`) — left for
the implementation plan.

## Open questions

- Exact serialization format and versioning for exported worker types.
- Whether depth-cap default (3) should be a global config or
  per-root-orchestrator override.
- How the ninox prompt disambiguates an action request from a read
  query when phrasing is ambiguous (left to the implementation plan,
  likely: no destructive/spawning action without the request containing
  an unambiguous imperative).
- Exact visual treatment for the context-sensitive controls row when no
  node is selected (default/global action set).

---

**Footnote (citation staleness, checked 2026-10-21 against `main`):**
`crates/ninox-core/src/messaging.rs` is now 496 lines, not the 518 cited
above.
