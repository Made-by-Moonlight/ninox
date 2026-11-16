# MLOPS-5339 follow-up: GUI Brain tab, PR3 — v1 Sources view

Spec: `BrainSources.dc.html` under `.claude/design-ref/fleet-overview-v1/`
(parent checkout), brief `BRIEF-MLOPS-5339-brain.md` section 3. Stacked on
PR2 (Graph, #269, still open); rebased onto `origin/v1` once that merges.

## Scope (PR3)

Replace the Sources placeholder with a real source-management view: a card
per `CatalogueRef` (name, health, type badge, location, entry count, last
sync, pending pushes, "Include in search"), an Add-source dashed card, and
a 380px edit panel (Name/Type/URL/region/endpoint/cache path/cadence/
access), writing to `[[brain.catalogues]]` through the existing
`persist_config` path. No secrets shown or stored anywhere — S3 uses the
ambient AWS profile/SSO, GitHub wiki uses ambient git credentials, matching
`brain_sync`'s existing `RemoteStore` implementations exactly.

### Core additions (`ninox-core::config::CatalogueRef`)

Three new scalar fields, additive and backward-compatible
(`#[serde(default = ...)]` so existing `config.toml` catalogues round-trip
unchanged until edited):
- `enabled: bool` (default `true`) — "Include in search".
- `cadence: String` (default `"manual"`) — stored preference only; no
  background scheduler (out of scope per the brief — "changing the sync
  engine's semantics").
- `access: String` (default `"read_write"`) — stored only; `BrainSync::sync`
  still always attempts both pull and push regardless, for the same reason.

Source *type* (Local folder / GitHub wiki / S3) stays derived from
`remote` (`None` / `is_github_wiki()` / else), not a new stored field — the
Edit panel's Type control drives which fields are shown and how `remote`
gets built, it doesn't duplicate state that already exists.

Health/error state is session-only on `App`, not persisted — `SyncState`
has no `last_error` field today, and GUI-only "last sync attempt failed:
<message>" is sufficient for the brief's red-card requirement without
touching the sync engine's on-disk format.

### GUI (`components/brain_sources.rs`, new)

- Card list + dashed "+ Add source" card, built from `app.catalogues`
  (plus a Spawn-style implicit "default" row, consistent with
  `catalogue_options()`).
- Per card: `brain_sync::remote_status()` (offline, no network) for
  cache-ttl/last-check/pending-pushes; a fresh `BrainIndex::open` + count
  for entries (small catalogues, acceptable to do on view-open/refresh,
  not per-frame).
- Edit panel: Name, Type seg-control, the one field that matters for that
  type (path / wiki URL / s3 bucket+prefix), region+endpoint only for S3,
  local cache path, Cadence/Access selects, Save / Sync now / Remove.
  Sync now calls the existing `BrainSync::for_brain(..).sync()` async via
  `Task::perform`, appending to a session-only "recent activity" log.
- Remove pops the `CatalogueRef` from config only — it does not touch the
  remote (brief, verbatim) and, per the narrowest reading of that line,
  doesn't touch local `.sync.toml`/cache files either.

## Out of scope for this PR (flagged for `request-work`, per the brief)

**Merged "All sources" index.** `BrainIndex`/`BrainEntry` have no concept
of which catalogue an entry came from, and the GUI's `BrainSwitchCatalogue`
fully replaces the open index rather than ever holding more than one. A
real merge needs a `source` field on `BrainEntry` (and whatever schema
change that implies) plus a union-query layer — sized separately rather
than folded into this PR, exactly as the brief instructs. List/Graph's
catalogue select keeps narrowing to one source at a time for now.

Also out of scope, per the shared brief: the TUI Brain tab, changing the
sync engine's semantics (no background scheduler, no read-only
enforcement), the ninox prompt.

## Test plan

- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.
- Unit tests for the new `CatalogueRef` fields' serde defaults, and for
  the pure helpers in `brain_sources.rs` (type-from-remote derivation,
  remote-string construction per type, form validation).
- Manual: isolated instance (scratch db/config/orchestrator root, real
  brain content), window-only screenshot of Sources with the add panel
  open, compared against `BrainSources.dc.html`.
