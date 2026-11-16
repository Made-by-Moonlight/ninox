# Hexagonal Boundaries for Session-Kind Behavior (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Captures a
proposed direction so an implementation plan can be cut from it later.
Lives on the long-lived `v1` branch.

**Implementation target:** `ninox-core` primarily, with call-site updates
in `ninox-app`/`ninox-server` as needed. File and line references below
reflect the code as of this writing; treat them as orientation, not
guaranteed locations.

## Context

Second spec in the next-gen refactor (first: fleet overview UI/UX
vision, merged to `v1`). This one addresses item 6 of the original
list: "better hexagonal patterns to make changes smaller in scope —
currently they're around 1000 lines even for simple changes."

Concrete evidence of the problem, gathered from this repo's own history:

- `crates/ninox-core/src/store.rs` is 1,681 lines, `types.rs` 694 lines,
  `messaging.rs` 518 lines — the three most-imported modules in the
  workspace are also the largest. 50 files across `ninox-core`,
  `ninox-app`, and `ninox-server` import `types::`/`store::` directly.
  Visibility inside `ninox-core/src` is almost entirely flat: 221 `pub`
  items vs. only 8 `pub(crate)` — essentially no internal encapsulation
  boundary exists today.
- Commit `fa23bd5` (MLOPS-4715, SSH remote-sessions transport) changed
  **25 files for 887 insertions** to add one new session kind. Most of
  that footprint is one-line touches to otherwise-unrelated modules just
  to make them aware the new kind exists: `events.rs` (+5),
  `fleet/snapshot.rs` (+1), `lib.rs` (+1), `lifecycle/poller.rs` (+1),
  `lifecycle/reactions.rs` (+1), `lifecycle/statusline.rs` (+1),
  `messaging.rs` (+1), `worker_status.rs` (+1) — plus real changes to
  `store.rs` (+44), `types.rs` (+8), `orchestrator_root.rs`, and a new
  329-line `remote.rs`. Textbook shotgun surgery from the lack of a
  port/adapter boundary around `Session`.
- Commit `c151750` (MLOPS-4670, sender identity on messages) touched 7
  files for one new field, spanning both `ninox-app` and `ninox-core`,
  and **broke the build one commit later** (`f63f7c8`, a 1-line fix) —
  a call site in `restart.rs` wasn't updated when `deliver_message`
  gained a new parameter. Direct evidence the coupling isn't just
  verbose, it's missable by the compiler today.
- Commit `28d9d7e` (detecting two more hook event types) added +352
  lines to `worker_status.rs` alone (471 total across 3 files) — a
  different flavor of the same root problem: one file doing too much,
  rather than cross-module fan-out.
- A working seam already exists and proves the pattern fits this
  codebase: `crates/ninox-core/src/runtime/mod.rs:105` defines
  `pub trait SessionBackend: Send + Sync`, implemented by
  `runtime/tmux_backend.rs`, `runtime/ptyd_backend.rs`, and
  `runtime/mock.rs`. This is a real, working ports-and-adapters boundary
  for the one subsystem (how a session executes) that has it. Everything
  else — lifecycle reactions, fleet snapshot shaping, status-line text,
  event emission — has no equivalent boundary; it special-cases session
  kinds by name across the modules listed above.

## Goals

- A new session/worker kind requires implementing one trait in one new
  file, not hand-editing ~8 existing modules.
- Shrink `Session`/`Store`'s public surface so most of the 50 current
  call sites go through narrow accessor functions instead of touching
  fields directly, turning a missed call site into a compile error
  instead of a runtime surprise (as happened in `c151750`/`f63f7c8`).
- Extend the proven `SessionBackend` pattern rather than introduce a
  new paradigm; stay inside the existing `ninox-core` crate.

## Non-goals

- Splitting the workspace into additional physical crates. Considered
  and explicitly rejected in favor of the lighter in-crate trait
  approach — see Design, below.
- Rewriting or "improving" `SessionBackend` itself. It already works and
  is the template this spec extends, not a target of this refactor.
- Any specific session kind's own feature work (e.g. the remote-sessions
  spec's 10 open questions) — this spec only changes how kinds plug in,
  not what any particular kind does.

## Design

### Scope decision: in-crate traits, not a crate split

Considered splitting `ninox-core` into several smaller crates (domain
types, store, messaging, lifecycle) with real Cargo dependency edges for
compiler-enforced one-directional coupling. Rejected for now: much larger
and slower effort, higher risk of a long-lived half-migrated state, and
`ninox-core` already has one proven lighter-weight pattern
(`SessionBackend`) that solves the same coupling problem without a
workspace restructure. Revisit a physical split later only if the
in-crate trait boundaries prove insufficient.

### Port 1: session-kind behavior hooks

A new trait (name/signature left to the implementation plan) sits
alongside `SessionBackend`, covering the concerns that currently
special-case session kinds by name: how a kind wants to be reacted to in
the lifecycle poller/reactions, how it shapes its fleet-snapshot
representation, what its status-line text looks like, and what events it
emits. Each session kind provides an implementation, naturally colocated
with its existing `SessionBackend` impl (`tmux_backend.rs`,
`ptyd_backend.rs`, a future `remote_backend.rs`, etc.).

The six call sites that currently special-case by kind
(`lifecycle/reactions.rs`, `lifecycle/poller.rs`,
`lifecycle/statusline.rs`, `worker_status.rs`, `fleet/snapshot.rs`,
`events.rs`) each change **once** to dispatch through the trait instead
of matching on kind. From then on, adding a new session kind means
implementing the trait in one new file, not editing six existing ones —
directly fixing the 25-file-for-one-feature problem demonstrated by
`fa23bd5`.

Whether this is a new, separate trait (interface segregation — a
backend implementor opts into only the hooks it needs) or additional
methods on `SessionBackend` itself (one richer trait) is left as an open
question below; existing `SessionBackend` implementors need to be
updated either way.

### Port 2: Session/Store encapsulation

A separate concern from Port 1: `store.rs` and `types.rs` are
god-objects with 50 files reaching in directly and almost no
`pub(crate)` boundary. This is about shrinking the public surface, not
dispatch — splitting `types.rs` into a lean core `Session` plus
per-kind extension data (rather than one struct with every kind's
fields crammed in), and tightening `store.rs`'s visibility so most call
sites go through narrow accessor functions. This is what would have
caught the `c151750`/`f63f7c8` missed call site at compile time instead
of needing a follow-up fix commit.

### Migration strategy

This touches a central, heavily-used module, so migrate incrementally:

1. Introduce the new trait(s) with default/fallback implementations that
   preserve current behavior exactly — this is a refactor, not a
   behavior change.
2. Migrate one of the six fan-out call sites at a time, smallest first
   (`lifecycle/reactions.rs` looks smallest per the `fa23bd5` diff
   stats), each as its own small, independently reviewable and
   mergeable PR against `v1`.
3. Do not attempt all six call sites in one PR — the whole point of this
   refactor is smaller changes, and its own implementation should
   demonstrate that rather than contradict it.
4. Port 2 (encapsulation) can proceed in parallel or after Port 1; it
   does not depend on Port 1's trait design.

### Testing

- Every migrated call site's existing tests must keep passing unchanged
  (behavior-preserving refactor, not a feature change).
- Add a test that a mock/stub session kind correctly implements the new
  trait and that each generic call site dispatches to it — replacing
  today's hand-written per-kind match arms with one generic test matrix
  that automatically covers future kinds.

## Open questions

- Exact trait name(s) and method signatures — left to the implementation
  plan.
- Whether Port 1 should be a new, separate trait (interface
  segregation) or additional methods on `SessionBackend` — leaning
  separate trait, but worth deciding explicitly before implementation
  since existing backends need updating either way.
- Exact sequencing of Port 1 vs. Port 2 across implementation PRs (this
  spec recommends Port 1 first, given it has the clearest and most
  costly evidence, but doesn't hard-require it).
