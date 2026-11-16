# Worker Types Backend (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch. Last item in the original backlog to receive a
design spec.

**Implementation target:** `ninox-core` (`harness.rs`-adjacent new
module, `store.rs` migration, `types.rs`, `capabilities.rs`), plus new
GUI/TUI management-screen code per item 7's design. File and line
references below reflect the code as of this writing; treat them as
orientation, not guaranteed locations.

## Context

Item 7's fleet-overview UI/UX vision spec (merged) designed worker
types as user-definable, extensible named configs (brief/prompt
template, tool/permission profile, default harness), with a list+form
management screen and export/import, explicitly deferring backend
storage/execution. Investigation found no `WorkerType` concept exists
anywhere today (`Session.agent_type` is the *harness* name, not a
role), but three existing patterns in this codebase cover almost all
of what's needed:

- **Registry shape**: `harness.rs`'s `HarnessSpec`/`HarnessRegistry`
  already does "compiled-in builtins, overlaid by `[harnesses.*]`
  config entries, synthesized fallback for unknown names" — and
  critically, `AppConfig::load()` is a stateless disk read with no
  cached singleton, so config-file changes already take effect without
  a restart. This is the template to follow for worker types.
- **Permission profile shape**: the worker-permission-model spec
  (merged, unimplemented) already defines
  `WorkerPermissionsConfig { enabled, allow, deny, ask }`, using Claude
  Code's own `permissions.allow/deny/ask` rule-string syntax verbatim,
  enforced both via a worktree-seeded `.claude/settings.json` block and
  a `PreToolUse` merge-gate hook. A worker type's "tool/permission
  profile" (item 7's term) is this same shape, scoped per type rather
  than global — not a second representation.
- **Storage migration shape**: `store.rs`'s existing idempotent
  `ALTER TABLE sessions ADD COLUMN` pattern (used for `model`,
  `catalogue_path`, `claude_session_id`, `machine_id`, etc.) is exactly
  how a new `worker_type TEXT` column on `Session` should be added.

What genuinely has no precedent: a **structured CRUD management UI**.
Every existing config-writing UI path in this codebase is a scalar
toggle via `AppConfig::update(|c| ...)` closures — nothing today writes
a named, multi-field, list-of-objects registry entry with validation,
let alone export/import. Item 7's list+form editor screen is real new
UI work, not a trivial wiring of an existing pattern.

`capabilities.rs`'s `Audience` (`Worker`/`Orchestrator`/`Both`) is
orthogonal to worker-type and needs no change — it's "which side of the
fleet," not "which kind of worker."

`ninox spawn` currently has no `--harness`/`--model`/`--type`-style
per-spawn flag at all; harness/model always come from global
`config.worker`. A worker-type flag is new CLI surface, not an existing
flag being repurposed.

## Goals

- Worker types are stored as `[[worker_types]]` entries in
  `config.toml`, following the same array-of-tables convention already
  used elsewhere (e.g. `[[brain.catalogues]]`), with a few compiled-in
  built-ins (Implementation, Researcher, Evaluator) as examples per item
  7's "not a closed set" requirement.
- A worker type's permission profile reuses the worker-permission-model
  spec's `WorkerPermissionsConfig` shape exactly, scoped per type.
- `ninox spawn --type <name>` resolves the named type's default
  harness/model/brief-template/permission-profile, overriding
  `config.worker`'s defaults for that spawn; omitting `--type` behaves
  exactly as today (unaffected, backward compatible).
- Export/import (item 7's explicit requirement) is just
  serializing/parsing one `[[worker_types]]` entry to/from a shareable
  snippet — no new format invented, reuses existing TOML/serde
  infrastructure.

## Non-goals

- The management UI's visual design — fully covered by item 7, not
  redesigned here. This spec covers the data model and spawn-time
  resolution the UI writes to and reads from.
- Changing `Audience` or the capability-registry's existing
  orchestrator/worker split — worker-type is a new, orthogonal
  dimension, not a modification to that enum.
- Enforcement mechanics beyond reusing the worker-permission-model
  spec's existing (unimplemented) design — this spec doesn't redesign
  how permissions are enforced, only how a worker-type's profile is
  stored and threaded into that existing mechanism.

## Design

### Registry: compiled-in builtins + config overlay, same as `harness.rs`

A new `WorkerTypeSpec` struct (name, brief template, default harness,
default model, `WorkerPermissionsConfig`), with a small set of
compiled-in builtins and a `WorkerTypeRegistry` that overlays
`[[worker_types]]` config entries on top — mirroring
`HarnessRegistry::from_config`'s whole-entry-replace-by-name semantics.
Loaded fresh on every `AppConfig::load()`, no caching, consistent with
how harness config already behaves (edits take effect without
restart).

### Storage: a new `Session.worker_type` column

`store.rs` gains an idempotent `ALTER TABLE sessions ADD COLUMN
worker_type TEXT` migration, paired with
`#[serde(default)] pub worker_type: Option<String>` on `Session`
(mirroring the existing `model` field). Recorded at spawn time,
unrelated to `agent_type` (which stays the harness name).

### Spawn-time resolution

`ninox spawn` gains a `--type <name>` flag. When given, the named
`WorkerTypeSpec` resolves the spawn's harness/model/permission-profile
*before* the existing `agent = config.worker.clone()` line, so the
type's defaults substitute for (not merge with) the global worker
config for that one spawn — the type is a complete override, not a
partial patch, avoiding ambiguity about field-by-field merge order.
The type's brief template, if any, composes with the `--prompt`
argument (exact composition — prepend, append, or required
placeholder — left to the implementation plan). An unrecognized `--type`
name is a clear spawn-time error, not a silent fallback to
`config.worker`.

### Permission profile enforcement

Reuses the worker-permission-model spec's existing (unimplemented)
enforcement design unchanged: the resolved type's
`WorkerPermissionsConfig` is seeded into the spawned worktree's
`.claude/settings.json` and backed by the same `PreToolUse` merge-gate
hook pattern, exactly as that spec already describes for the
non-worker-type case — this spec only changes *where the config comes
from* (a named type instead of the single global
`[worker_permissions]`), not how enforcement works.

### Export/import

A worker type is just one `[[worker_types]]` TOML table. Export
serializes one named entry to a standalone snippet (TOML, matching the
config file's own format — no new serialization format introduced);
import parses and appends one entry, validating it against
`WorkerTypeSpec`'s shape (same validation a hand-edited config entry
would need to pass) before accepting it.

### Management UI (new work, no precedent to follow)

Explicitly flagged rather than hand-waved: today's codebase has no
existing pattern for a UI that writes a structured, named,
multi-field registry entry — every config-writing UI path is a scalar
toggle. Item 7's list+form editor screen is genuinely new GUI/TUI code,
reading/writing the `[[worker_types]]` array via the registry described
above. This spec's job is to make sure that UI has a clean data model
to read and write; building the screen itself is implementation work,
not re-designed here (item 7 already specified its visual shape).

## Testing

- `WorkerTypeRegistry` builtin+override merge test, mirroring
  `HarnessRegistry`'s existing test coverage shape.
- `ninox spawn --type <name>` resolution test: harness/model/permission
  profile come from the named type, not `config.worker`; an
  unrecognized type name errors clearly.
- Export/import round-trip test for one worker type entry.
- Migration test: the new `worker_type` column is added idempotently,
  consistent with the existing `ALTER TABLE` migration tests.

## Open questions

- Exact composition rule for a type's brief template vs. the `--prompt`
  argument (prepend/append/placeholder) — left to the implementation
  plan.
- Whether built-in worker types (Implementation/Researcher/Evaluator)
  should be overridable by a same-named `[[worker_types]]` config
  entry, or protected/reserved names — not resolved here.
- Interaction with the nested-orchestration spec's permission gating
  for the nested-spawn capability (PR #199) — that spec already notes
  worker-type profiles are the natural place to grant/withhold
  nested-spawn permission; this spec's `WorkerTypeSpec`/
  `WorkerPermissionsConfig` is the concrete shape that reconciliation
  should use, but the reconciliation itself is left to whichever spec
  implements first.
