# Statusline-Sourced Context & Cost Tracking

## Context

Ninox launches the real `claude` CLI interactively inside a tmux pane per
session (`harness::HarnessRegistry::interactive_cmd`/`worker_cmd`), so there
is no request/response boundary Ninox controls where cost or context-window
usage could be captured directly. Today (`crates/ninox-core/src/lifecycle/
usage.rs`) Ninox works around this by re-reading `claude`'s own on-disk
transcript JSONL files after the fact, summing token counts across every
transcript file found for a session's workspace directory, and estimating
`$` cost against a small hand-maintained price-per-model table. This has two
known problems:

1. **The price table is a guess**, not derived from anything authoritative,
   and goes stale silently for new/renamed models (unrecognized models
   silently fall back to Sonnet-tier pricing).
2. **Multi-file summation has no dedup.** A workspace "may accumulate
   multiple transcripts across resumed/restarted `claude` invocations"
   (existing doc comment, `usage.rs:38-40`), and every turn in every file is
   summed with no dedup by turn ID — a real risk of double-counting if a
   resumed session's transcript ever replays earlier turns.

Separately, `context_tokens` is derived as "latest turn's input +
cache_creation + cache_read," which has no relationship to the model's
actual context window size or Claude Code's auto-compact buffer — it cannot
answer "how close is this session to needing a compact," only "how big was
the last turn."

Claude Code's `statusLine` hook (docs: <https://code.claude.com/docs/en/statusline>)
solves both problems at the source: Claude Code invokes a configured command
after each assistant turn (event-driven, debounced 300ms, plus an optional
fixed-interval refresh) and pipes it a JSON payload containing, among other
fields:

- `cost.total_cost_usd` — a live, cumulative cost figure computed by the
  `claude` binary itself for the running process's session lifetime. Still
  client-side estimated (not an invoice), but authoritative in the sense
  that it is Anthropic's own tracked figure, not a locally-guessed price
  table re-derived from raw token counts.
- `context_window.used_percentage` / `remaining_percentage` — pre-calculated
  percentage of the context window used, accounting for the model's actual
  window size (`context_window.context_window_size`, 200k or 1M depending on
  model).
- `context_window.total_input_tokens` / `total_output_tokens` — current
  (not cumulative) context-window occupancy.
- `model.display_name`, `workspace.current_dir`, `cost.total_duration_ms`.

No other Claude Code hook event receives `context_window` data — confirmed
by inspecting `open-gsd/gsd-core`'s own statusline hook
(`hooks/gsd-statusline.js`), which has to write a bridge file from its
`statusLine` hook to a separate `PostToolUse` context-monitor hook
specifically because `PostToolUse` doesn't receive this data itself. So
getting this data at all means configuring a `statusLine` command.

## Goal

Ninox configures its own `statusLine` command (not gsd's, not `ccusage`, no
external dependency) for every session it spawns. That command feeds
`context_window.used_percentage`/token counts and `cost.total_cost_usd`
straight into Ninox's own session store, becoming the primary source for
both fields in the UI — with the existing transcript-based ingestion kept
as a fallback for the window before the hook has fired at all.

## Non-goals

- No IPC/push mechanism between the hook process and the running GUI
  process. Ninox has no existing push path (no Unix socket, no gRPC); every
  existing CLI subcommand (`ninox spawn`, `ninox request-work`, ...) is a
  short-lived process that writes into the same WAL-mode SQLite store the
  GUI reads, and the GUI picks up external changes on its next poll tick.
  This feature follows that exact precedent rather than introducing new
  infrastructure (e.g. a new `ninox-server` HTTP endpoint + port-discovery
  file) for a UI field that isn't latency-critical.
- No `Session.id` / Claude session-UUID correlation work. A `--session-id`/
  `claude_session_id` pairing exists only on an unmerged branch
  (`feat/resume-interrupted-sessions`) and isn't needed here: the hook
  payload's `workspace.current_dir` is matched against `Session.workspace_path`,
  the exact mechanism `usage::ingest_usage_for_workspace` already uses today.
- No reconciliation logic for cost resetting across a `claude` process
  restart. Per Anthropic's own docs, `/resume`/`--resume` restores
  cumulative cost rather than resetting it (a reset-on-resume bug exists
  only in the Cursor/VS Code extension, not the standalone CLI Ninox
  drives), so this is a non-issue as long as session restarts go through
  `--resume` rather than a bare new `claude` invocation.
- No changes to the visible terminal statusline's exact copy/layout beyond
  what's needed to not look broken (see "Visible statusline output" below)
  — the real display surface for this data is Ninox's own UI.
- No git-branch/PR-badge/rate-limit fields from the hook payload, even
  though they're available for free in the same JSON. Out of scope for this
  pass; can be added later without a redesign since the subcommand already
  parses the full payload.

## Architecture

### New `ninox statusline` subcommand

A new subcommand on the existing `ninox` binary (`crates/ninox-app/src/
main.rs`, alongside `spawn`/`send`/`request-work`/`brain`). Invoked with no
arguments; reads the hook's JSON payload from stdin, same shape documented
at <https://code.claude.com/docs/en/statusline#available-data>.

Following the existing split between `ninox-core` (business logic) and
`ninox-app` (CLI/GUI wrappers) — the same shape `usage.rs`/`poller.rs`
already have — the payload-parsing and store-update logic lives as pure,
directly-testable functions in a new `crates/ninox-core/src/lifecycle/
statusline.rs` module (parallel to `usage.rs`). `main.rs`'s subcommand
handler is a thin wrapper: read stdin, call into `ninox-core`, print the
returned line, exit 0. Tests in "Testing" below call the `ninox-core`
functions directly, never spawning the subprocess.

Flow:

1. Parse stdin as JSON. On any parse failure, print a minimal fallback line
   (see below) and exit 0 — never a non-zero exit or empty stdout, both of
   which blank the visible statusline per Claude Code's own docs.
2. Extract `workspace.current_dir` (fall back to `cwd` if absent).
3. Open the same SQLite store the other CLI subcommands already open
   directly (same `AppConfig`-resolved db path as `run_spawn`); WAL mode
   already makes this safe for concurrent multi-process writers.
4. `store.list_sessions()` and find the session whose `workspace_path`
   matches — same correlation Ninox already relies on in `usage.rs`. No
   match (stray directory, or a race where the hook fires before the
   `Session` row exists yet) → skip the store write, still print a line.
5. From the payload, pull (each independently optional — see "Null
   handling" below):
   - `context_window.used_percentage` → `context_used_pct: Option<f64>`
   - `context_window.total_input_tokens` → `context_total_tokens: Option<u64>`
   - `context_window.context_window_size` → `context_window_size: Option<u64>`
   - `cost.total_cost_usd` → `cost_usd: f64` (only overwrites if present and
     not null)
   - `model.display_name` → `model: Option<String>` (only overwrites if
     `session.model` is currently unset, matching existing `poll_usage`
     behavior)
6. Upsert the session with whichever fields were present; fields absent in
   this payload are left untouched (not zeroed).
7. Print the visible statusline text to stdout (see below).

### Null handling

Per Claude Code's docs, `context_window.current_usage` is `null` before the
first API call and again immediately after `/compact` until the next API
response; `used_percentage`/`remaining_percentage` "may be null early in the
session." The subcommand treats every hook field as independently optional:
a null/absent field is a no-op for that field, never a zero write. This
mirrors the existing `parse_turn`'s tolerant style in `usage.rs`.

### Visible statusline output

A single line, kept short and cheap to compute (no `git` shell-outs, no
network calls — Claude Code's own docs warn that slow scripts block the
line from updating and get cancelled mid-run if a new update fires):

```
[Opus 4.8] 📁 my-worktree | ▓▓▓▓▓▓░░░░ 62% | $2.60
```

- Model + directory basename always shown.
- Context bar + `cost` shown only when the corresponding payload field is
  present (both independently — e.g. cost may be present before the first
  context percentage is, per the null-handling rules above).
- Color thresholds match Claude Code's own documented convention for
  context bars: green under 70%, yellow 70–89%, red 90%+.
- No session/store data is required to print this — it's computed straight
  from the parsed payload, so the line still renders correctly even when
  the workspace-to-session lookup misses.

### Settings wiring

Ninox writes `.claude/settings.json` in exactly one place today:
`setup_orchestrator_root` (`crates/ninox-app/src/app.rs`, ~line 2368),
called once per orchestrator root, alongside the existing `PreToolUse`
subagent-blocker hook. Worker sessions — spawned into their own worktree by
`create_worker_worktree` (`crates/ninox-app/src/spawn_util.rs`) — get no
`.claude/` config at all today; they simply inherit whatever's already in
the repo. Since worker sessions are exactly the ones doing the token-heavy
work, this feature needs to add a settings write to both places, not just
extend the existing one:

1. `setup_orchestrator_root`'s existing settings JSON gains a `statusLine`
   entry alongside the `PreToolUse` hook:

   ```rust
   let settings = serde_json::json!({
       "hooks": { /* unchanged */ },
       "statusLine": {
           "type": "command",
           "command": format!("{ninox_bin} statusline"),
           "refreshInterval": 20
       }
   });
   ```

2. `create_worker_worktree` gains a new best-effort step after creating the
   worktree: write a minimal `.claude/settings.json` containing only the
   `statusLine` key (no hooks — the subagent-blocker hook is an
   orchestrator-only concern and isn't part of this feature's scope for
   workers) when none already exists.

Both writes are guarded by "only write when absent" — a pre-existing
`.claude/settings.json` (orchestrator root, or a worktree branch that
already carries one) is never touched.

This is added only inside the existing `if !settings_path.exists()` guard —
identical to today's behavior, a pre-existing `.claude/settings.json` (e.g.
checked into a branch) is never touched. `refreshInterval: 20` re-runs the
command every 20s even when the session is idle (e.g. an orchestrator
blocked on subagents), on top of Claude Code's event-driven updates on each
turn — cheap, since the subcommand does one SQLite read/write and exits.

### Store schema

Additive columns on `Session` (`crates/ninox-core/src/store.rs`), no
migration of existing data required:

```sql
ALTER TABLE sessions ADD COLUMN context_used_pct REAL;
ALTER TABLE sessions ADD COLUMN context_total_tokens INTEGER;
ALTER TABLE sessions ADD COLUMN context_window_size INTEGER;
```

`cost_usd` and `context_tokens` (existing columns) are unchanged in shape;
the hook simply becomes a second writer of `cost_usd`, taking precedence
per "Fallback semantics" below.

### Poller: detecting external writes

Every existing `Poller` write path (`poll_pids`, `poll_usage`, `poll_github`)
follows a read-diff-upsert-emit cycle the poller itself drives. The
statusline hook is a new kind of writer: an external short-lived process
that writes directly into the store outside the poller's control, same as
`ninox request-work` already does for work-request files — except those are
picked up via `sync_sessions_metadata`'s explicit file read, and there's no
equivalent "check the store for external session-row changes" step today.

Add a small diff cache to `Poller`, same shape as its existing
`enrichment_cache: Arc<Mutex<HashMap<SessionId, EnrichmentState>>>`, keyed
by session ID, storing the last-seen `(cost_usd, context_used_pct,
context_total_tokens)` tuple. On the existing 5s `pid_interval` tick
(alongside `poll_pids`, not a new interval), re-read `list_sessions()`,
compare each session's tuple against the cache, and `emit(Event::
SessionUpdated(session))` + update the cache entry for anything that
changed. This is the same tick Ninox already uses for its fastest-latency
check today (PID liveness), so this reuses an existing cadence rather than
adding a fourth interval.

### Fallback semantics

`usage.rs`/`poll_usage` (10s interval, transcript-based) is **not removed**.
It remains the source for:

- Sessions where the hook hasn't fired yet (workspace trust dialog not yet
  accepted, or no turn sent — the `Session` row exists but no statusline
  data has arrived).
- `context_tokens` as it exists today, kept for any UI/code path not yet
  migrated to the new `context_used_pct`/`context_total_tokens` fields.

Once the hook has written a non-null value for a field, that field's
statusline-sourced value takes display precedence over the transcript-
derived one — the UI reads `context_used_pct` when present, falling back to
computing a rough percentage from `context_tokens` only when it is not
(e.g. immediately after spawn, before the first turn).

## UI

`crates/ninox-app/src/components/inspector_panel.rs`'s `format_burn`
becomes:

```
$2.60 · 62% context (124k/200k)
```

falling back to today's `$2.60 · 214k tokens` when `context_used_pct`/
`context_window_size` are absent (pre-first-turn, or hook never fired for
this session). The same cost value (now hook-sourced when available) is
already displayed as `${:.2}` in `fleet_board.rs`, `pr_list.rs`, and
`session_detail.rs` — no format change needed there beyond the value now
coming from a fresher source once the hook has fired.

## Error handling

- The subcommand must never hang or panic: any I/O error (store open
  failure, malformed JSON, no matching session) degrades to printing the
  minimal `[Model] 📁 dir` fallback line and exiting 0.
- SQLite WAL mode already tolerates concurrent single-writer contention from
  multiple short-lived subcommand invocations across different sessions;
  no additional locking is introduced.
- A session that never sends a first turn (spawned, then abandoned) simply
  never gets statusline data — same as it never gets transcript-based usage
  data today. No special-cased handling needed.

## Testing

- Pure-function tests for payload parsing (JSON → extracted optional
  fields), covering: full payload, `context_window`/`cost` entirely absent,
  `current_usage`/`used_percentage` explicitly `null`, malformed JSON.
- Store-level test mirroring `poll_usage_ingests_transcript_into_store_and_
  emits_update`: seed a session with a `workspace_path`, invoke the
  subcommand's core update function directly (not via subprocess) against a
  tempdir SQLite store, assert the new columns are set.
- Test that a workspace with no matching session is a no-op on the store
  but still produces non-empty stdout.
- Poller test for the new diff-cache tick: seed two sessions, mutate one's
  `context_used_pct` directly via the store (simulating an external hook
  write), tick the poller, assert exactly one `SessionUpdated` event fires
  for the changed session and none for the unchanged one.
- Settings-generation test: assert a freshly written `.claude/settings.json`
  includes the `statusLine` key, and that an already-existing settings.json
  is left byte-for-byte unchanged (extends whatever existing test covers
  the `if !settings_path.exists()` guard, if one exists — otherwise a new
  test alongside it).
- `format_burn` unit tests for both the new (`context_used_pct` present)
  and fallback (`context_tokens`-only) cases.
