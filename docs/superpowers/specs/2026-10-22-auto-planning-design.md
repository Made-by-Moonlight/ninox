# Automatic Planning and Sectioning (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-core` (new module, e.g.
`lifecycle/auto_plan.rs`), `config.rs` (new `[planning]` section). File
and line references below reflect the code as of this writing; treat
them as orientation, not guaranteed locations.

## Context

Item 9 of the next-gen refactor. The fleet-overview UI/UX vision spec
(item 7, merged) already designed the *review and approval* UI for a
generated plan — ghost nodes (proposed, not-yet-running), editable
brief/worker-type/dependencies per section, Approve & spawn
all/selected/Add section/Discard controls, rendered inline at every
zoom level. That spec explicitly deferred the *generation* side,
noting that "separate specs will cover the auto-planning/sectioning
algorithm, the hexagonal/module-boundary refactor of `ninox-core`, and
the expanded CLI surface." The nested-orchestration spec (merged, PR
#199) similarly excludes "the
auto-planning/sectioning algorithm that decides *when* to spawn a
sub-orchestrator" from its own scope. This is that spec.

Confirmed via investigation: this is fully greenfield. The existing
`orchestrator_plans` table (`store.rs`, shipped per
`docs/superpowers/specs/2026-08-26-orchestrator-plan-tracking-design.md`)
is a pointer to a human-authored markdown file on disk for the `plan`
skill's live-rendering UI — no sections, no worker-type/dependency
fields, not a data model to extend. No raw LLM API integration exists
anywhere in the codebase.

The one real architectural precedent: `lifecycle/brain_harvest.rs`
already runs a scoped, non-interactive agent subprocess (`claude -p
--dangerously-skip-permissions --tools Bash,Read,Write,Edit`) for a
narrow one-shot task — extracting facts from a diff into the brain.
This is a working middle ground between "ninox makes a raw model API
call" and "ninox spawns a full interactive worker session," and this
spec follows the same shape for planning.

### The output contract (already fixed by item 7, not renegotiated here)

Per the fleet-overview spec's "Plan review (ghost nodes)" section, a
generated plan must produce, per proposed section: a **brief** (text),
a **worker type** (one of the registered, possibly user-defined, worker
types from that same spec), and **dependencies** on other sections
(implying a DAG/ordering). Critically, a section can itself propose a
**nested sub-orchestrator** owning further sub-sections (the
fleet-overview spec's own "Nested orchestration and the depth cap"
section describes exactly this kind of delegation, and ghost nodes for
a pending plan render inline at every zoom level, nested children
included) — so the output shape is a tree, not a flat list.

## Goals

- A scoped, non-interactive planning subprocess (following
  `brain_harvest.rs`'s pattern) takes a task description and produces a
  structured plan tree matching item 7's ghost-node contract exactly —
  no new UI-facing data shape invented here.
- The planning subprocess gets **read-only codebase exploration**
  (Read/Grep against the target workspace, explicitly no Write/Edit/
  Bash) — proposing sections informed by the codebase's actual shape,
  not guessed blind from task text alone.
- Planning runs against a **separate, independently configurable
  planning model** — not necessarily the orchestrator's own configured
  harness/model — via a new `[planning]` config section, following the
  existing `[orchestrator]`/`[worker]` harness/model config pattern
  already in `config.toml`.
- A generated plan is always a *proposal*. It never auto-executes;
  item 7's existing review/approval flow is the only path from a
  generated plan to real spawning. This spec produces ghost nodes for
  review, nothing more.

## Non-goals

- Any UI work — fully covered by item 7, reused as-is.
- Auto-approval or auto-execution of a generated plan.
- Iterative/multi-turn re-planning loops — a single-shot proposal is
  this spec's scope; refinement happens through item 7's existing
  manual edit/add-section/discard review controls, not a second
  automatic pass.
- Fine-tuning or training a dedicated planning model — "separate,
  configurable planning model" means a config choice among existing
  models/harnesses, not training anything new.

## Design

### The planning subprocess

A new, scoped, non-interactive agent subprocess, architecturally
parallel to `brain_harvest.rs`: given a task description and a target
workspace path, it runs with a tight system prompt instructing it to
explore (Read/Grep only — no Write/Edit/Bash, since planning must never
modify anything) and then emit a structured JSON plan tree matching
item 7's contract (brief/worker-type/dependencies per section,
sections nestable under a proposed sub-orchestrator). Harness and model
for this subprocess come from a new `[planning]` config section
(`harness`, `model`), independent of `[orchestrator]`/`[worker]` —
mirroring the existing config pattern, not introducing a new one.

### Validation before the plan ever reaches review UI

The planner's raw output is untrusted, same posture as every other
external/model-produced input this refactor has treated carefully
(e.g. item 2's identity spoofing fix, item 10's sender attestation):

- JSON must parse and match the expected schema exactly, or planning
  fails cleanly (reported error, no plan shown) rather than passing
  through malformed or partial output.
- Every referenced worker type must exist in the worker-type registry
  (item 7) — an unrecognized worker type is a validation failure, not
  silently defaulted.
- The proposed tree's depth must not exceed the nested-orchestration
  spec's configured depth cap (default 3, PR #199) — a plan proposing
  a deeper structure is rejected at validation time, not trimmed or
  silently flattened. The planner's own system prompt should be told
  the cap so this is rare in practice, but validation does not trust
  the prompt alone to enforce it.
- Dependency references between sections must form a valid DAG (no
  cycles) — a cyclic dependency is a validation failure.

Only a plan that passes all of the above is handed to item 7's existing
ghost-node review UI. Nothing here bypasses or weakens that review step
— validation rejects bad *input* to the review UI; it doesn't replace
human/orchestrator review of valid plans.

### Triggering

Reuses item 7's already-decided trigger: the ninox prompt recognizing a
multi-section/epic request, or an orchestrator's own decision to plan
before spawning. This spec does not add a new trigger surface — it's
the thing that runs once triggered.

## Testing

- A test harness with a stubbed/mocked planning subprocess (returning
  canned JSON) to test the validation layer in isolation: schema
  violations, unknown worker types, over-depth trees, and dependency
  cycles are all rejected with clear errors, never passed through.
- An integration-style test (real subprocess, a small fixture
  repository) confirming a simple task produces a valid, well-formed
  plan tree end to end.

## Open questions

- Exact JSON schema / system prompt wording for the planning
  subprocess — left to the implementation plan.
- Whether `[planning]` should have its own default (e.g. always falls
  back to `[orchestrator]`'s config if unset) or must be explicitly
  configured before auto-planning is available at all — not resolved
  here.
- Cost/rate-limiting considerations for a planning call that explores
  a large codebase — not addressed here.
