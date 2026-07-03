# Athene → Ninox User Migration

## Overview

Athene ([slievr/Athene](https://github.com/slievr/Athene)) is the existing Node.js CLI/web dashboard for orchestrating agent fleets. Ninox is a from-scratch Rust/Iced rewrite of the same concept. Existing Athene users need a way to bring their fleet state (orchestrators, sessions, linked PRs) into Ninox without re-registering every project by hand or losing history.

This is a **one-time, repeatable import**, not a live compatibility layer — the two tools don't need to run against shared state, and Ninox should keep working after a user uninstalls Athene entirely.

## Data model mapping

Athene state on disk (per-user):

- `~/.agent-orchestrator/config.yaml` — global config; `projects.<projectId>` gives `path`, `repo.{owner,name,originUrl}`, `displayName`.
- `~/.agent-orchestrator/projects/<projectId>/sessions/<sessionId>.json` — one file per session. `role: "orchestrator"` marks the project's coordinator session; siblings are workers. Carries `lifecycle.session.state`, `lifecycle.pr.{state,number,url}`, `agent`, `createdAt`.
- `<projectId>/code-reviews/`, `worker-prompt-*.md`, `worktrees/` — Athene-specific artifacts with no Ninox equivalent. **Not migrated.**

Ninox store (`crates/ninox-core/src/store.rs`): `orchestrators`, `sessions`, `prs`, `ci_status`, `review_comments` tables, typed via `crates/ninox-core/src/types.rs`.

| Athene | Ninox | Notes |
|---|---|---|
| project's `role: "orchestrator"` session | `Orchestrator { id, name, created_at }` | `id` = Athene session id, `name` = project `displayName` |
| sibling sessions in the project | `Session { orchestrator_id = <above>, ... }` | |
| `config.yaml` `repo.{owner,name}` | `Session.repo` | joined by `projectId`, formatted `"owner/name"` |
| `lifecycle.session.state` + `lifecycle.pr.state` | `SessionStatus` | mapping table below |
| `lifecycle.pr.{number,url}` | `Session.pr_number`, `PR.number`/`url` | `PR.title`/`body` are not stored by Athene locally — left empty; Ninox's live GitHub polling fills them in after import |
| — | `Session.cost_usd` | Athene doesn't track per-session cost today; imported as `0.0` |
| — | `ci_status`, `review_comments` | not persisted by Athene (fetched live from GitHub); left empty, backfilled by Ninox's own tracker after import |

### Status mapping

Source enums, from `packages/core/src/types.ts`:

- `CanonicalSessionState`: `not_started | working | idle | needs_input | stuck | detecting | done | terminated`
- `CanonicalPRState`: `none | open | merged | closed`
- `CanonicalPRReason`: `not_created | in_progress | ci_failing | review_pending | changes_requested | approved | merge_ready | merged | closed_unmerged | cleared_on_restore`

Mapping is evaluated in this order (first match wins), using `lifecycle.pr.reason`/`lifecycle.pr.state` and `lifecycle.session.state`:

| Condition | Ninox `SessionStatus` |
|---|---|
| `pr.reason == "merged"` | `Done` |
| `session.state == "terminated"` (and PR not merged) | `Terminated` |
| `pr.reason == "ci_failing"` | `CiFailed` |
| `pr.reason in (review_pending, changes_requested)` | `ReviewPending` |
| `pr.reason in (approved, merge_ready)` | `Mergeable` |
| `pr.state == "open"` (none of the above matched) | `PrOpen` |
| `session.state == "not_started"` | `Spawning` |
| anything else (`working`, `idle`, `needs_input`, `stuck`, `detecting`) | `Working` |

This is the full, exhaustive decision table — no unmapped enum values remain.

## Precondition

Athene must have completed its own storage migrations (i.e. the user has run `athene start` at least once on a current version) before export. This means the exporter only ever reads the current `projects/<projectId>/sessions/*.json` layout — it does not need to know about the legacy hash-based (`{12-hex}-{projectId}`) layout that `packages/core/src/migration/storage-v2.ts` already handles.

## Export (Athene side)

New CLI command: `athene export --for-ninox <output-path.json>` (package: `packages/cli`, backed by new logic in `packages/core/src/export/`).

Output is a single versioned JSON document:

```json
{
  "formatVersion": 1,
  "exportedAt": "2026-07-03T12:00:00.000Z",
  "orchestrators": [
    { "id": "ath-orchestrator", "name": "athene", "createdAt": 1750000000 }
  ],
  "sessions": [
    {
      "id": "ath-17",
      "orchestratorId": "ath-orchestrator",
      "name": "ath-17",
      "repo": "slievr/Athene",
      "status": "review_pending",
      "agentType": "claude-code",
      "startedAt": 1750000000,
      "prNumber": 23,
      "prUrl": "https://github.com/slievr/Athene/pull/23",
      "workspacePath": "/Users/.../worktrees/ath-17"
    }
  ]
}
```

Walks every project in `config.yaml`, reads its `sessions/*.json`, resolves `repo` from the project's config entry, and applies the status mapping above. Projects with no `repo.originUrl` (e.g. local-only) export `repo` as the `displayName` verbatim.

## Import (Ninox side)

New module: `crates/ninox-core/src/import/athene.rs`.

```rust
pub struct ImportSummary {
    pub orchestrators_imported: usize,
    pub sessions_imported: usize,
    pub skipped: Vec<String>, // ids skipped due to unparseable status, with reason
}

pub fn import_athene_export(store: &Store, path: &Path) -> Result<ImportSummary>;
```

Deserializes the export JSON, maps each entry to `ninox_core::types::{Orchestrator, Session}`, and calls the existing `Store::upsert_orchestrator` / `Store::upsert_session`. These are already `ON CONFLICT DO UPDATE`, so importing the same file twice (or a newer export over an older one) is safe — same Athene session id always maps to the same Ninox row.

Unknown/unmapped `formatVersion` values are rejected with an error naming the supported version(s); unparseable individual entries are skipped and reported in `ImportSummary.skipped` rather than aborting the whole import.

Exposed via a new route in `crates/ninox-server`: `POST /api/import/athene { "path": "..." }` → `ImportSummary`.

## Trigger (Ninox app)

An "Import from Athene" action in `ninox-app`, surfaced from the sidebar/settings area: opens a file picker defaulting to `~/.agent-orchestrator` (or a raw export JSON path), calls the server route, and shows the returned `ImportSummary` (counts + any skipped entries) as a toast/notification. No separate CLI flag on `ninox-app` — the app is GUI-first and this is a rare, one-time action, so a second entry point would be unused surface area.

## Out of scope

- Athene ↔ Ninox live coexistence or shared runtime state.
- Migrating `code-reviews/`, worker prompt files, or worktrees — no Ninox feature consumes these.
- CI status and review comment history — not persisted by Athene; Ninox's own live tracker repopulates these post-import once a session's PR is linked.
- Per-session cost history — not tracked by Athene today.
- Automating the export step from within Ninox (e.g. Ninox shelling out to `athene export` itself) — the user runs the Athene-side export command manually and points Ninox at the resulting file.
