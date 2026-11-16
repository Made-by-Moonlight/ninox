# MLOPS-5475: ninox-gui panics on shutdown

## Root cause

`ninox-gui`'s `main` was `#[tokio::main] async fn main()`. Its whole body —
including the call to `iced::application(...).run_with(...)` that drives the
winit event loop — runs inside the outer tokio runtime's `block_on` frame
(`run_with` isn't `.await`ed; it's a plain synchronous, blocking call made
from inside that polled future).

`run_with` builds and, when the event loop ends and it returns, drops its own
internal tokio runtime (`iced_futures::runtime::Runtime<tokio::runtime::runtime::Runtime,
...>`). Tokio refuses to drop any `Runtime` while the current thread is still
inside another runtime's `block_on` — doing so blocks the thread to join
worker/blocking-pool threads, which tokio disallows from within an async
context — and panics instead:

```
thread 'main' panicked at .../tokio-1.52.3/src/runtime/blocking/shutdown.rs:51:
Cannot drop a runtime in a context where blocking is not allowed. This
happens when a runtime is dropped from within an asynchronous context.
```

This fires on **every** exit from the event loop — a normal window close,
Cmd+Q, or the explicit `iced::exit()` after `Message::RestartApp` — not just
on some unusual path. The app is a single-window `iced::application` with no
secondary windows and no other place that ends the event loop (grepped for
`iced::window::close`, `CloseRequested`, `iced::exit()`: only the one
intentional call, from the restart flow). So there's no separate "event loop
exits unexpectedly" bug to fix — the event loop exits exactly when the user
quits, and that already-correct exit is what panics. That's why the user saw
it as "the GUI keeps crashing" during normal use: quitting is normal use.

## Fix

Make `main` synchronous. Build one explicit `tokio::runtime::Runtime`, use
`rt.block_on(...)` only for the async setup (`dispatch`, `bootstrap` — which
also starts long-running tasks like the poller via `tokio::spawn`, so `rt`
must stay alive, just never re-entered, for the GUI's whole lifetime), then
call `iced::application(...).run_with(...)` directly from `main`'s
synchronous body — never inside a `block_on` frame. When iced's internal
runtime is later dropped, the thread isn't marked as "inside a runtime", so
the drop succeeds.

`ninox-ui-shared::cli::dispatch`'s doc comment assumed its caller was always
`#[tokio::main] async fn main()`; updated it to describe both callers
(ninox-tui unchanged, ninox-gui now via `Runtime::block_on`).

`ninox-tui` was audited for the same pattern (`Runtime::new`, `Builder::new_`,
`block_on`) and is unaffected: it's a plain `#[tokio::main]` binary with no
nested runtime — ratatui doesn't build its own tokio runtime the way iced's
winit backend does.

## Verification

- `cargo test -p ninox-gui`: all tests pass, including two new regression
  tests in `main.rs` that isolate the exact tokio mechanism (no iced/winit
  involved): dropping a second runtime after an outer `block_on` call has
  already returned succeeds; dropping one from inside an outer runtime's
  `block_on` panics with "Cannot drop a runtime" (reproducing the crash's
  root cause directly).
- `cargo clippy -p ninox-gui -p ninox-ui-shared --all-targets -- -D warnings`:
  clean.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`: green after rebasing onto origin/v1.
- Live-verified in an isolated instance (fresh `--db`, scratch
  `config.toml`/`NINOX_BRAIN`/`NINOX_TMUX_SOCKET`/`TMUX_TMPDIR`, `env -u` for
  every `NINOX_*` var, no real store/brain touched), `RUST_BACKTRACE=1`. The
  window was closed through its own close button via an Accessibility
  `AXPress` action (no synthetic mouse input); it is the app's only window.
  - Fixed build: exit code `0`, stderr has no panic, no new `.ips` under
    `~/Library/Logs/DiagnosticReports`.
  - Same procedure against origin/v1's `main.rs`: exit code `101` with the
    exact crash panic (`tokio .../blocking/shutdown.rs:51`, "Cannot drop a
    runtime in a context where blocking is not allowed").
