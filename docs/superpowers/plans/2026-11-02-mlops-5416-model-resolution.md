# MLOPS-5416 follow-up: one model rule, plus a configurable Ninox type

Two PRs off `origin/v1`: A (model resolution) first, then B (Ninox type).

## PR A: one model rule, applied everywhere

### Resolver (`ninox_core::models`)

`effective_agent(&SpawnModelRequest, &AppConfig) -> ResolvedAgent { agent, source }`
with `ModelSource = Explicit | Type | Setting | HarnessDefault`.

Harness:
- Worker role: explicit harness, else the worker type's harness (when a
  type is named), else `[worker].harness`.
- Orchestrator role: explicit harness, else `[orchestrator].harness`.

Model, first match wins:
1. Explicit model (Spawn dialog pick the user changed, `--model`, an
   orchestrator passing one, or the row's model on restore/resume/restart).
2. The worker type's `model`, if set and the type's harness is the
   resolved harness.
3. The role setting (`[orchestrator].model` / `[worker].model`), if set and
   that role's harness is the resolved harness. Model ids are
   harness-specific, so a mismatched setting is skipped rather than passed
   to the wrong CLI.
4. Built-in role default: `HARNESS_ROLE_DEFAULTS` (one table; on
   claude-code Opus 5.5 for orchestrators, Sonnet 5.5 for everything else).
5. Harness default: the harness spec's `model`, or no `--model` flag.

`[planning].model` is a one-shot plan run, not a session; `models show`
lists it but the resolver doesn't use it.

### Spawn paths converted
| Path | Request |
|---|---|
| `ninox spawn` (no `--type`) | Worker, Implementor type, harness pinned to `[worker].harness`, `--model` explicit |
| `ninox spawn --type T` | Worker, type T, `--model` explicit |
| `spawn-orchestrator` / `spawn-sub-orchestrator` / `orchestrate` | Orchestrator, Orchestrator type, `--model` explicit |
| GUI / TUI Spawn dialog | Kind's role + chosen type; pick is explicit only if the user changed it |
| #320 queue payloads | Resolved once at admit, frozen in the payload |
| Restore / re-file / resume / restart-in-place / queue Resume+Relaunch | Row's harness+model, kept as-is |
| Prompt agent fresh start | Worker role, `prompt` type |

Removed: the GUI dialog's write-back of its pick into `[orchestrator]`
("remembered preselection"), and `relaunch_agent`'s config-over-row rule.

### UI / CLI
- Settings › Agents "Default models" block (GUI and TUI): Orchestrator and
  Worker model, harness-aware picker + custom field, saved via
  `AppConfig::update`, with a note that types and per-spawn picks override.
- Spawn dialog line: `Model: X (from setting / type / explicit / harness default)`.
- Session header shows the model.
- Worker Types rows: `model: inherits Worker default (X)` when unset.
- `ninox models show [--json]`: resolved model per role and per type.
- `ninox spawn` / `spawn-orchestrator` / `spawn-sub-orchestrator` gain `--model`.

### Tests
Resolver unit test per precedence case; one test per spawn path asserting
the launched command carries the resolved `--model`; restore keeps the row
model; live scratch check (Worker model X → pane has `--model X`).

## PR B: Ninox prompt agent as a configurable built-in type
Visible built-in "Ninox" type (renaming the hidden `prompt` type), badged
"System · single instance". Editable harness/model/brief addendum/allow/ask;
locked name, delete, spawnability, seeded deny rules and `can_manage_types`.
Still one `ninox-prompt` session via the #324 lease; changes apply on next
start. Detailed plan written when A is up.
