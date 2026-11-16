# `ninox config` Command (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-ui-shared/src/cli.rs` (new `Command`
variant), `ninox-core` (config read/write helpers). File and line
references below reflect the code as of this writing; treat them as
orientation, not guaranteed locations.

## Context

Item 3 of the next-gen refactor, originally phrased broadly ("breaking
out ninox capabilities into more CLI commands for agents to use").
Investigation found the actual gap is narrow: of the capability
registry's 21 entries (`capabilities.rs`) and the CLI's 37+ existing
`Command` variants, every capability maps cleanly to a real command
except one. `set-agent-config` (orchestrator-only, gates changing the
orchestrator's/worker's configured harness or model) has rich skill
documentation (`skills/orchestrator/set-agent-config.md`) but no CLI
backing at all — the skill instructs the agent to print the raw config
file path and format, then "Use the Edit tool to update the relevant
field" directly. No other capability in the registry has this gap; no
evidence was found elsewhere of agents being told to touch `ninox.db`
directly, run raw SQL, or hand-craft JSON instead of calling a command.

## Goals

- A real `ninox config` command replaces the Edit-tool instruction for
  `set-agent-config`, so an agent changes `[orchestrator]`/`[worker]`
  harness/model config the same structured way it does everything else
  in this registry.
- Generalize slightly beyond just harness/model: support reading and
  writing arbitrary `config.toml` keys by dotted path, so this doesn't
  need re-designing the next time a new top-level config section is
  added.
- Writes are safe under concurrent access — `config.toml` is a single
  shared file multiple sessions could plausibly touch.

## Non-goals

- A general config *validation* framework beyond what's needed for the
  harness/model case specifically (e.g. this doesn't attempt to
  validate arbitrary future config sections' semantics, just read/write
  mechanics).
- Any other capability — this spec is scoped to the one confirmed gap,
  not a general CLI-surface redesign (the rest of the surface was
  found to already be complete).

## Design

### Command shape

```
ninox config get <dotted.key>              # print current value
ninox config set <dotted.key> <value>      # write, read-modify-write
ninox config show                          # dump full resolved config
```

`set-agent-config`'s existing use case becomes, e.g.:
`ninox config set orchestrator.harness codex` /
`ninox config set worker.model <model-name>`.

### Validation

For the known `[orchestrator]`/`[worker]` harness field specifically:
validate the value against the harness registry (`harness.rs`) the same
way spawn-time harness selection already does, rejecting an unknown
harness name with a clear error rather than writing it and failing
later at spawn time. For arbitrary other dotted keys (the generalized
case), no semantic validation beyond "is this valid TOML for this
position" — matches this spec's non-goal of not building a general
validation framework.

### Concurrency safety

`set` performs a read-modify-write: read the current file, apply the
change, write to a temp file, atomic rename over the original — not an
in-place partial write. This is a minimum bar (prevents a torn/
corrupted file from a crash mid-write); true multi-writer race safety
(two concurrent `ninox config set` calls) is left to the implementation
plan to decide whether it needs a stronger guarantee (e.g. a file lock)
given how rarely concurrent config writes are expected to actually
happen in practice.

### Authorization

Gated the same way `set-agent-config` already is in the capability
registry (orchestrator-only) — the implementation should use the same
`orchestrator_auth`-style caller verification the spawn-auth fix
(nested-orchestration spec) applies elsewhere, rather than leaving this
command unauthenticated, consistent with closing rather than
reproducing that class of gap.

## Testing

- `ninox config get`/`set` round-trip test for a known key.
- Concurrent-write test: two sequential (simulated) `set` calls to
  different keys both land, neither clobbers the other.
- Unknown-harness-name rejection test for the validated `orchestrator.
  harness`/`worker.harness` case specifically.

## Open questions

- Full set of keys worth exposing beyond `orchestrator.*`/`worker.*` —
  left to the implementation plan to enumerate from `AppConfig`'s
  actual shape.
- Whether a stronger concurrency guarantee (file lock) is warranted —
  not resolved here.
