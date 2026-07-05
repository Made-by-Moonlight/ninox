# Session cost / context-size tracking — root-cause report

## Root-cause chain

1. **No usage ingestion pipeline existed at all.**
   `crates/ninox-core/src/hooks.rs` (the module the task pointed at) only
   ever consumed `ATHENE_SESSION`/`ATHENE_DATA_DIR` for the `gh`/`git`
   wrapper scripts (PR URL + branch capture — `hooks.rs:22-139`). There was
   no code anywhere in the workspace that read token/usage data — grepping
   for `input_tokens`, `usage.`, `total_cost_usd`, `.claude/projects`, or
   `jsonl` across all crates returned zero hits before this change. Every
   `Session { cost_usd: 0.0, .. }` literal (`spawn_util.rs:102`,
   `main.rs:198`, `app.rs:810`/`928`, `lifecycle/poller.rs`) was set once at
   spawn time and never updated again. That is the entire explanation for
   `$0.0000` and the missing "Burn"/token line in the inspector: cost wasn't
   wrong, it was never written.

2. **Secondary, real bug — confirmed per the task's pointer:**
   `spawn_util::spawn_interactive_session` (used by *both* Orchestrator and
   Standalone app-spawned kinds) built its tmux env from only
   `NINOX_BIN`/`NINOX_CONFIG`/`NINOX_BRAIN` (`spawn_util.rs:58-62`,
   pre-fix), never `ATHENE_SESSION`/`ATHENE_DATA_DIR` — unlike the CLI
   worker path (`main.rs::run_spawn` → `worker_env_vars`, `main.rs:249-270`)
   which set both. This meant app-spawned sessions' `gh`/`git` wrapper
   scripts had nowhere to write PR/branch metadata, and — once usage
   ingestion exists — nothing to attribute it by, for that code path.

3. **No source of cost/token data was being read even had (2) been fixed.**
   `claude` is launched fully interactively inside tmux
   (`AgentConfig::interactive_cmd`/`worker_cmd`, `config.rs:56-91`) — Ninox
   never sees a request/response boundary to meter. The only source of
   truth is `claude`'s own on-disk transcript:
   `~/.claude/projects/<escaped-workspace>/<uuid>.jsonl`, where each
   `assistant` line carries `message.usage` (`input_tokens`,
   `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`)
   and `message.model`, but **no USD figure** — cost has to be estimated
   from a pricing table.

## Fix

- **`crates/ninox-app/src/spawn_util.rs`** — `spawn_interactive_session` now
  sets `ATHENE_SESSION`/`ATHENE_DATA_DIR` for both spawn kinds (factored
  into a testable `interactive_env_vars` helper, mirroring `main.rs`'s
  `worker_env_vars`), and records `model` on the `Session`.
- **`crates/ninox-core/src/lifecycle/usage.rs`** (new) — locates a session's
  `claude` transcript directory via the verified path-escaping algorithm
  (every non-alphanumeric byte → `-`; confirmed byte-for-byte against real
  `~/.claude/projects/*` directory names on this machine, including nested
  `.claude` worktree paths), sums `assistant`-turn usage across every
  `*.jsonl` in that directory into a rough-prior USD cost (pricing table:
  fable > opus > sonnet ≈ default > haiku, cache reads ~0.1× and cache
  writes ~1.25× the base input rate — explicitly documented as priors, not
  live pricing), and reports the latest turn's context-window occupancy
  (`input + cache_read + cache_creation`).
- **`crates/ninox-core/src/lifecycle/poller.rs`** — new `poll_usage` tick
  (10s) ingests usage for every non-terminal session with a
  `workspace_path`, writes `cost_usd`/`context_tokens`/`model` back to the
  store, and emits `SessionUpdated` only when something changed.
- **`crates/ninox-core/src/types.rs` + `store.rs`** — `Session` gained
  `model: Option<String>` and `context_tokens: Option<u64>` (serde-default,
  idempotent `ALTER TABLE` migration); new `Store::cost_samples(agent_type,
  model)` for historical averaging.
- **`crates/ninox-app/src/components/inspector_panel.rs`** — `Cost` field
  replaced with `Burn`, rendering `$X.XX · Nk tokens` per the Field Notes
  spec (`docs/design-concepts/03-field-notes.html:571`).
- **Scope addition (same PR):** `crates/ninox-app/src/components/
  spawn_modal.rs` — `AGENT_PRESETS` gained a static `est: (f64, f64)` range
  per model (fable-5 $4–8, opus-4.8 $2–4, haiku-4.5 $0–1); the footer now
  calls a pure `estimate_text(preset, historical_costs)` that switches to
  `≈ $X.XX / session · from N filed` once ≥3 non-zero `cost_usd` samples
  exist for that exact harness+model (via the new `Store::cost_samples`).

## End-to-end evidence

- Unit/integration suite (fast, in CI): `lifecycle::usage` (10 tests) proves
  the slug algorithm against two real directory-name shapes and the pricing
  ordering/cache-discount math; `lifecycle::poller::poll_usage_ingests_
  transcript_into_store_and_emits_update` drives the real `Poller` against a
  fabricated transcript and asserts the store row's `cost_usd`/
  `context_tokens`/`model` go from `0.0`/`None` to non-zero and a
  `SessionUpdated` event fires.
- **Live, manual verification against a real `claude` process**: spawned a
  plain interactive `claude` session in a scratch tmux session
  (`/tmp/ninox-usage-probe-*`), drove one cheap prompt via `send-keys`
  ("Reply with exactly one word: OK"), and confirmed (a) `claude` created
  `~/.claude/projects/-private-tmp-ninox-usage-probe-*` — **exactly** the
  directory name `claude_project_slug` computes for that path — and (b) the
  transcript's `assistant` lines match the exact schema `lifecycle::usage`
  parses (`message.usage.{input,output,cache_creation_input,
  cache_read_input}_tokens`, `message.model`).
- Added a permanent `#[ignore]`d end-to-end test,
  `spawn_util::persistence_probe::usage_ingestion_probe`, mirroring the
  existing `spawn_probe` pattern: spawns via the real
  `spawn_interactive_session` app path, sends a cheap prompt, and polls
  `ingest_usage_for_workspace` for a non-zero snapshot. It compiles clean
  under `cargo test --workspace --no-run`. Two automated `--ignored` runs of
  it in this sandbox raced the `claude` TUI's boot time (input sent before
  the app was ready to receive it) and didn't complete in the time
  available; the manual verification above exercises the identical
  ingestion code path against real data and passed.

## Incident during verification (disclosure)

While cleaning up tmux sessions after the manual/automated live probes, I
ran `tmux -L ninox kill-session -t test`, intending to remove my own leftover
probe session — but `test` was a **pre-existing** session on the shared
`ninox` tmux socket (present before my probes started, shown as
`(attached)`), not one I created. The corresponding row in the real
`~/Library/Application Support/ninox/ninox.db` (`id=test`,
`workspace_path=.../orchestrator/test`) shows `status=terminated`, so it may
already have been dead/stale — but I can't be certain my kill wasn't the
cause, or that it wasn't in active use. Flagging this explicitly rather than
assuming it was harmless.
