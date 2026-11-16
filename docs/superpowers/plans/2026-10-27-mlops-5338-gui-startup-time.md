# MLOPS-5338: ninox-gui time-to-window is still multiple seconds

## Context

MLOPS-5188 (#246) stopped `App::new` from eagerly shaping a
`text_editor::Content` per persisted review comment, which previously pegged
the main thread and meant no window ever appeared. After that fix a window
does appear, but still takes multiple seconds against the real data dir
(5,563 review comments / ~12MB of bodies, 80 sessions at measurement time).

## Investigation

Instrumented `main()`/`run_gui()`/`bootstrap()`/`App::new()` with ad-hoc
`Instant` timestamps (removed before landing) and cross-checked against
`sample` and `RUST_LOG=info` output. Findings, in order of execution:

1. `App::new`'s own DB loads (`list_orchestrators`, `list_sessions`,
   `list_comments` grouping into `review_threads`, theme loading,
   `resize_terminals`) cost ~40–50ms total — already small after #246.
2. `run_gui`'s call to `bootstrap()` (`crates/ninox-ui-shared/src/cli.rs`)
   runs entirely *before* `iced::application(...).run_with(...)` is even
   constructed, and was the real bottleneck:
   - `setup_orchestrator_root(...).await` — tens of ms of file I/O.
   - `FastEmbedEmbedder::try_new` via `spawn_blocking` — loading the
     tokenizer + ONNX session for brain semantic search. Observed
     150ms–500+ms depending on disk-cache/CPU contention.
   - `port_in_use(...).await` (deciding whether to also bind the
     in-process server) plus spawning the poller/server.
   None of this is needed to construct `App::new`'s arguments or render a
   frame — `UiBootstrap` never even carries the embedder.
3. After the boot closure returns, iced's own pipeline (wgpu
   adapter/device/surface setup, and especially `cosmic-text`'s
   first-use system-font scan) costs several hundred ms in a debug build.
   This is inside the `iced`/`iced_wgpu`/`cosmic-text` dependencies — no
   `cosmic_text`/`fontdb`/`FontSystem` reference exists anywhere in
   `ninox-gui`/`ninox-ui-shared` — and is not addressed here.

## Fix

Restructure `bootstrap()` so the headless daemon path (`ninox-tui
--headless`, and the no-display fallback) keeps its exact prior behavior —
it *is* the poller/server, so it still awaits all of this setup directly —
while the interactive GUI path treats the embedder, the already-running
probe, and the poller/server spawn as fire-and-forget background work
(`tokio::spawn`), since nothing the window renders depends on any of it.
This removes the dominant cost (the embedder) from the GUI's critical path.

`setup_orchestrator_root(...).await` stays synchronous, awaited exactly
where it always was, for both paths — a first draft of this change moved
it into the same deferred block, but review caught two problems that made
that wrong: (1) it ran after the fallible `BrainIndex::open(...)?`, so a
`brain` open failure would skip orchestrator-root seeding entirely, on
*both* paths, where it previously ran unconditionally first; (2)
`orchestrator_root.rs` has a reachable `.expect(...)` — deferred into a
detached `tokio::spawn`, a panic there would be silently swallowed instead
of crashing the process loudly as it did before. Its own cost (tens of ms)
isn't the bottleneck anyway, so it wasn't worth the risk.

`App::new`'s own DB loads are left as they were: `list_orchestrators`/
`list_sessions` are already synchronous by design (so the UI isn't empty on
restart) and cheap; `list_comments`/`review_threads` is also intentionally
synchronous (so a restart doesn't blank the Marginalia feed until the next
poll — see the existing comment from #246) and, at ~40ms, isn't the
bottleneck.

## Verification

- `cargo test -p ninox-ui-shared`: 216 passed (no behavior change for the
  headless/daemon path; no new testing seam needed since `bootstrap()` has
  no pre-existing unit coverage of its own — it's exercised via manual GUI
  verification, same as #246).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- Built the real binary (debug and release) against the live local store
  and measured launch-to-window time via `osascript 'tell application
  "System Events" to count windows of ...'`, before and after, see PR body.

## Notes

- No design spec existed under `docs/superpowers/specs` for this ticket.
- The remaining debug-build floor (iced/cosmic-text's own startup pipeline)
  is outside `ninox-gui`/`ninox-ui-shared` and out of scope.
