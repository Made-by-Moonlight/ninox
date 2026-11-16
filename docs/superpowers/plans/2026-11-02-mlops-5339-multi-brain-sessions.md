# MLOPS-5339: multiple brains per session (read many, write one)

## Model

`ninox_core::brain_set::BrainSet { reads: Vec<PathBuf>, write: PathBuf }`.
`write` is always in `reads`, and it must be writable (`CatalogueRef::access != "read_only"`).
Names and read-only status are looked up in config at use time and never stored on the row.

- **Default (nothing chosen):** reads every catalogue with "Include in search" on, and writes to `default` (`resolved_brain_path`, which still honours a terminal-level `NINOX_BRAIN`).
- **Inheritance:** a child gets its parent's set verbatim. `--brain` / `--brain-write` can narrow it, but only to the parent's reads (`BrainSet::narrow`). A human caller (no session) may pick any configured catalogue (`BrainSet::select`).
- **Caller identity:** the parent set comes from the caller's session row, resolved through `orchestrator_auth::resolve_live_caller_session_id`. `NINOX_BRAIN_READS` / `NINOX_BRAIN` are never trusted inside a session.

## Persistence

- `Session.catalogue_path` is replaced by `brain_reads: Vec<String>` + `brain_write: Option<String>`. `catalogue_path` is accepted as a serde alias.
- Store: new columns `brain_reads` (JSON array) and `brain_write`.
  - Reads use `brain_write`, falling back to the legacy `catalogue_path`, so rows written before this change, or by an older binary, become single-catalogue sets.
  - Every insert and update **dual-writes** `catalogue_path = brain_write`, because older daemons still read that column for harvest. Remove the dual-write once no shipped binary reads `catalogue_path`.
- Queue payloads (`WorkerLaunch`, `OrchestratorLaunch`, `StandaloneLaunch`):
  - gain `brain: Option<BrainSet>`;
  - keep `ninox_brain` / `catalogue_path` = write path for older drainers;
  - a payload without `brain` becomes `BrainSet::single(...)`.
- Env: `NINOX_BRAIN` = write path. `NINOX_BRAIN_READS` = JSON array of read paths, parsed defensively. It is scrubbed by ptyd.

## Surfaces

- **CLI:**
  - `brain query/show/related/backlinks` use `MergedBrainIndex` over the read set, with `source:id` ids. A single-brain set uses its `BrainIndex` directly, so output is unchanged. `--catalogue` narrows to one brain.
  - `brain add [--catalogue]` goes through `BrainSet::authorize_write`.
  - `index/sync [--all]`: writable brains get a full sync, read-only ones `pull_if_stale`.
  - `import`, `remote set` and `remote unset` are authorized against the write catalogue.
  - `spawn`, `spawn-orchestrator` and `spawn-sub-orchestrator` take `--brain` and `--brain-write`.
  - `list --json` adds `brain: {reads, writes}` names.
- **Harvest:** `Poller::trigger_brain_harvest` writes to the set's authorized write catalogue and skips it if that catalogue is read-only.
- **Resume / refile / restore / prompt agent:** `BrainSet::from_session`.
- **GUI/TUI Spawn dialogs:** a "Brains this tree reads" multi-select with read-only badges, plus a "Writes to" choice limited to the writable selected brains (`spawn_form::brain_choices` / `reconcile_write`).
- **"Brains: reads … · writes …":** shown in the GUI header breadcrumb row, the side panel, the Info tab and the orchestrator island detail; in the TUI, in the inspector Info tab and island detail.
- **Skills:** the orchestrator and worker `brain.md` files and the spawn skills are updated.

## Tests

- `brain_set` unit tests: default, select, narrow/widen refusal, authorize, row-vs-env.
- `store` tests: round-trip, dual-write, legacy migration.
- Harvest routing in the poller.
- Refile and resume keep the set.
- Queue payload round-trip and legacy payload handling.
- CLI query fan-out with source tags, add routing and refusals, single-brain parity.
- TUI form multi-select; shared `brain_choices`.
