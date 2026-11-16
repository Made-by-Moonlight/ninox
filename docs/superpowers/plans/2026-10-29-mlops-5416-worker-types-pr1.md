# MLOPS-5416 PR1: seeded worker types, `ninox worker-type` CLI, `spawn --type`

Implements `docs/superpowers/specs/2026-10-22-worker-types-backend-design.md`
plus the ticket brief's additions (hidden flag, `can_spawn`, the
`Implementor` rename). PR2 (GUI Worker Types tab) is a separate PR on this
ticket, done after this one lands.

## What already exists on `v1` (not duplicated here)

- `WorkerTypeSpec`/`WorkerTypeRegistry` (`ninox-core/src/worker_type.rs`):
  builtins `Implementation`/`Researcher`/`Evaluator`, config overlay,
  `compose_prompt`, `resolved_agent`, TOML export/import for one entry,
  `allow_nested_spawn`.
- `Session.worker_type` column + store migration, `AppConfig.worker_types`
  + `upsert_worker_type`/`remove_worker_type`/`worker_type_registry()`.
- `ninox spawn --type <name>` resolution in `run_spawn` (cli.rs):
  unknown-type error, full-override agent/prompt resolution, recorded on
  the session.
- A full TUI Worker Types list+form editor
  (`ninox-tui/src/tui/worker_types.rs`, wired into the Types tab) — list,
  edit, save/reset, export/import already shipped under MLOPS-5350. Not
  rebuilt here; only its list is filtered for `hidden` (below).
- `authorize_nested_spawn`/`authorize_nested_spawn_as_worker`
  (orchestrator_auth.rs): precedent for resolving a worker's own
  `worker_type` from its session row, reused conceptually for `can_spawn`.

## Naming (final, 29 Oct)

Canonical built-in name is **Implementor** (not "Worker" — "worker" is
generic). Accepted aliases: `Implementation` (pre-existing), `Worker`
(in case anything already used it). Alias resolution is case-exact,
one level (alias → canonical), done entirely inside
`WorkerTypeRegistry` — `AppConfig.worker_types` itself stays a raw,
alias-oblivious `Vec<WorkerTypeSpec>` (no change needed to
`upsert_worker_type`/`remove_worker_type`).

## 1. `worker_type.rs` changes

- `WorkerTypeSpec` gains:
  - `hidden: bool` (`#[serde(default)]`) — excludes a type from
    `WorkerTypeRegistry::user_facing_names()` and from `ninox spawn --type`
    regardless of caller.
  - `can_spawn: Vec<String>` (`#[serde(default)]`) — type names this type
    may spawn, or `["*"]` for "every user-facing type". Empty = none.
  - `allows_spawning(&self, canonical_requested_name: &str) -> bool`.
- `builtin_specs()` becomes: `Orchestrator`, `Implementor`, `Researcher`,
  `Evaluator`. Each gets a brief template (Orchestrator: none — it's never
  spawned through `ninox spawn`), default harness, and a permission
  profile matching current behavior (all four ship with permissions
  disabled, matching today's unenforced status quo — the
  worker-permission-model spec's enforcement is still unimplemented).
  `can_spawn`: Orchestrator `["*"]`, Implementor `["Researcher"]`,
  Researcher `[]`, Evaluator `[]`.
- `ALIASES: &[(&str, &str)] = &[("Implementation", "Implementor"), ("Worker", "Implementor")]`,
  a `canonical_name(name: &str) -> String` helper.
- `WorkerTypeRegistry::from_config` canonicalizes every override's name
  before inserting (so a legacy `[[worker_types]] name = "Implementation"`
  override still lands on the one `Implementor` entry). `get()` resolves
  through `canonical_name` first. `names()` unchanged (canonical only, no
  alias duplicates — already true once overrides are canonicalized).
  New `user_facing_names()` filters `hidden`.
- A small `validate_permission_rule`/`validate_type_name` pair (no
  external regex dependency — name must be non-empty and
  ASCII-alphanumeric/`-`/`_` only; a rule must be a non-empty tool name,
  optionally followed by a balanced `(...)`), used by the CLI's
  create/edit/import.

## 2. `ninox worker-type` CLI (new `cli.rs` subcommand + handlers)

```
ninox worker-type list [--json]
ninox worker-type show <name> [--json]
ninox worker-type create --name <n> --brief <text|@file> [--harness H] [--model M]
                          [--allow RULE]... [--deny RULE]... [--ask RULE]...
                          [--can-spawn "A,B"|"*"]
ninox worker-type edit <name> [--brief ...] [--harness ...] [--model ...]
                        [--allow RULE]... [--clear-allow] (same for deny/ask)
                        [--can-spawn "..."] [--clear-can-spawn]
ninox worker-type delete <name>     # refuses built-ins (incl. aliases), suggests reset
ninox worker-type reset <name>      # removes the config overlay
ninox worker-type export [<name>...]   # TOML to stdout
ninox worker-type import <file>
```

- `create`/`edit` resolve `--brief @file` by reading the file.
- `create` validates: name non-empty/slug-safe, not already used (by
  canonical name or alias) unless editing, each rule passes
  `validate_permission_rule`, `can_spawn` entries are either `*` or a name
  that resolves in the registry at write time (built-in or already
  defined custom — lets `can_spawn` reference a type created moments
  earlier).
- `edit` starts from the *effective* resolved spec (builtin defaults +
  any existing overlay) and applies only the flags given, then
  `AppConfig::update` + `upsert_worker_type`.
- `delete`/`reset` operate on `AppConfig.worker_types` (raw, pre-alias)
  by resolving the given name to whatever literal entry (if any) matches
  it or its canonical form; `delete` on an unmodified or built-in name
  (by alias or canonical) is refused with "use reset instead" wording for
  built-ins and a plain "no such custom type" for a name that was never
  customized.
- `list`/`show` use `user_facing_names()` (hidden types never show) and
  mark built-ins (`name` present in `builtin_specs()`'s canonical set)
  with a `(built-in)` suffix in the human output; `--json` emits the full
  spec including `can_spawn`/`hidden`.
- `--help` text on every subcommand is written assuming the only reader
  is an agent that has never seen this command before (MLOPS-5415 will
  rely on it) — explicit about alias behavior, `*`/comma-separated
  `can_spawn`, and the built-in delete/reset split.

## 3. `ninox spawn` integration

- `run_spawn` gains a `caller_session_id: Option<String>` parameter
  (resolved once in `dispatch()` from `NINOX_SESSION`, passed down —
  mirrors how `orchestrator_id`/`caller: CallerRuntime` are already
  resolved outside `run_spawn` to keep it a pure, directly testable
  function of explicit inputs).
- Resolution, before any side effect:
  1. `requested_type_name = worker_type.clone().unwrap_or("Implementor")`.
     Always resolved through the registry (even with no `--type`), so
     hidden/Orchestrator/can_spawn checks apply uniformly; an unknown
     name errors listing `user_facing_names()`.
  2. Reject a hidden requested type, and reject `Orchestrator` outright
     (spawning one goes through `spawn-orchestrator`/
     `spawn-sub-orchestrator`, never `ninox spawn`).
  3. `caller_worker_type = resolve_caller_worker_type(&store, caller_session_id)`:
     `None` with no calling session (unrestricted — a bare human/GUI/TUI
     call). Otherwise `Some("Orchestrator")` when `store.is_orchestrator`
     is true for that id (**every existing orchestrator session predates
     the `worker_type` column and has it unset — this check must key off
     `is_orchestrator`, never off the column, or the entire existing
     fleet would be misclassified as Implementor and locked out of
     spawning anything but Researchers**), else the session's own
     `worker_type` column, defaulting to `"Implementor"` when unset.
  4. If `caller_worker_type` is `Some(t)`, `t`'s resolved spec's
     `can_spawn` must allow the requested type, else a clear error naming
     both the caller's type and its `can_spawn` list.
  5. Agent/prompt resolution: **unchanged from today when `--type` is
     omitted** (`config.worker`, prompt untouched) — only the *recorded*
     `worker_type` defaults to `Implementor`. This is the brief's
     "untyped spawns default to Implementor" reconciled with the spec's
     own "omitting `--type` is unaffected, backward compatible": the
     label changes, the resolved harness/model/template do not. Noted
     explicitly in the PR description per the "note it in the PR" rule
     for a spec/brief tension. (Confirmed with the orchestrator before
     implementing.)
  6. Parent resolution: an `Orchestrator` caller (or no caller session)
     keeps today's exact `--orchestrator-id`/`NINOX_ORCHESTRATOR_ID` +
     `authorize_orchestrator` verified-claim flow, unchanged. A
     non-orchestrator caller that just cleared its `can_spawn` check is
     parented to **itself** (`caller_session_id`), ignoring any inherited
     `NINOX_ORCHESTRATOR_ID` (that names its own parent, not where the
     new sub-session belongs) and skipping `authorize_orchestrator`
     entirely — no pane-identity verification for this path, by design:
     `can_spawn` is a policy guardrail (what roles may create what),
     not a trust boundary like claiming to *be* a verified orchestrator.
- `spawn_orchestrator_common`'s `Session { worker_type: None, .. }` becomes
  `Some("Orchestrator".to_string())` (covers both `spawn-orchestrator` and
  `spawn-sub-orchestrator`, which both call it).

## 4. Orchestrator guidance

Update `crates/ninox-core/skills/orchestrator/spawn-worker.md`'s existing
"Worker Types" section: built-in names are now Orchestrator/Implementor/
Researcher/Evaluator (keep the `Implementation` alias mention), and add a
one-line pointer to `ninox worker-type --help` for defining custom types.

## 5. TUI list filtering

`WorkerTypesView::reload` (`ninox-tui/src/tui/worker_types.rs`) switches
from `reg.names()` to `reg.user_facing_names()`, so a future hidden
`prompt` type never appears in the tab's list. No other TUI changes —
its editor already exceeds this ticket's "read-only list" ask from
MLOPS-5350; not re-scoped here.

## Tests

- `worker_type.rs`: builtins present (incl. Orchestrator, hidden=false
  for all four), alias resolves both ways, `user_facing_names` excludes
  hidden, `allows_spawning` matrix (wildcard, exact, empty, unrelated
  name), name/rule validators.
- `config.rs`: existing upsert/remove tests untouched (they operate on
  the raw vec); one new test confirming an override keyed by a legacy
  alias name still overlays the canonical entry through
  `worker_type_registry()`.
- `cli.rs` (`run_spawn` + new worker-type handlers): the `can_spawn`
  matrix from the brief — Orchestrator spawns anything non-hidden;
  untyped *orchestrator* caller (not just a typed one) can still spawn an
  untyped (Implementor) worker; Implementor spawns Researcher and the
  child is parented to the Implementor's own id; Implementor denied
  Evaluator; Researcher/Evaluator denied everything; no-session (bare)
  call is unrestricted; hidden type always refused; `--type Orchestrator`
  always refused. `worker-type` CLI create/edit/delete/reset/export/import
  round-trip and validation-error cases.
