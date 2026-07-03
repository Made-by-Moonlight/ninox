# Native Terminal Rendering via Attached tmux Clients

**Date:** 2026-07-03
**Status:** Approved design, pending implementation plan

## Problem

The terminal view in `ninox-app` does not behave like a native terminal:

- **Multi-line input is broken.** Shift+Enter (and other modifier-encoded keys) never
  reach the inner app. Input is injected via `tmux load-buffer` + `paste-buffer`, which
  cannot carry modifier-encoded key sequences, and the app's key encoder does not
  implement the extended keyboard protocols (kitty CSI-u / modifyOtherKeys) that TUIs
  like Claude Code use to distinguish Shift+Enter from Enter.
- **Scrollback is unreliable.** Content scrolled off by cursor-addressed TUI redraws is
  reconstructed by diffing successive frames (`extra_history` / `detect_shift` in
  `crates/ninox-app/src/components/terminal.rs`). The heuristic is inherently lossy —
  spinner chrome, reflows, and frame-boundary guesses confuse it — and the last several
  commits are all patches to it. Scrollback is also lost across reattach.
- **General rendering glitches.** Root cause: two terminal emulators consume diverging
  data. tmux emulates each pane authoritatively, while the app's alacritty `Term`
  re-emulates a side-tap (`pipe-pane` → FIFO) that misses everything before the tap
  started, and reattach relies on a "bounce resize" hack to force a repaint. The user's
  `~/.tmux.conf` also leaks into ninox sessions.
- **Renderer approximations.** Cell size is a hardcoded `0.6/1.4 × font_size` guess,
  the base-16 palette is hardcoded, and cursor shapes / underline styles are not
  implemented.

## Goals

1. Rendering as close to a native terminal as practical, supporting the full TUI
   feature set (extended keyboard, mouse reporting, bracketed paste, truecolor,
   cursor shapes, synchronized output).
2. Reliable scrollback, including across app restarts / session reattach.
3. Keep tmux as the mux — session persistence/restore is a hard requirement.
4. Keep the browser WebSocket terminal route and background-session monitoring working.

## Decision

**The app becomes a real tmux client.** For each session currently on screen, the app
opens a PTY pair and spawns `tmux attach -t <session>` on the slave side; the master
side feeds the existing alacritty `Term`. From tmux's perspective Ninox is
indistinguishable from iTerm: full repaint on attach, reflow on resize, mode
passthrough, and query/response handled by the party that owns the state. tmux's pane
grid and history are the single source of truth; the app renders them.

### Dedicated tmux server

Ninox runs its own tmux server on a private socket: `tmux -L ninox -f <config>`, where
the config is written by ninox to a fixed path at every startup (no drift between app
versions). The user's `~/.tmux.conf` no longer applies. Config contents:

```
set -g default-terminal "tmux-256color"
set -as terminal-features "xterm-256color:RGB:usstyle:extkeys:hyperlinks"
set -s extended-keys always
set -s extended-keys-format csi-u
set -g history-limit 100000
set -g status off
set -s escape-time 0
set -g window-size latest
set -g allow-passthrough on
set -g focus-events on
```

`extended-keys always` + `extended-keys-format csi-u` + the `extkeys` feature flag is
the Shift+Enter fix: the inner app's kitty-protocol negotiation flows through tmux to
the app's emulator and back. Minimum tmux version: **3.2** (checked at startup with a
clear error; 3.6a is current locally).

**Migration:** sessions created by older builds live on the default tmux socket and
will not appear on the ninox socket. tmux cannot move a session between servers, so:
new sessions are always created on the ninox socket; session discovery polls both
sockets; a legacy session found on the default socket is attached without `-L ninox`
(it keeps the user's tmux config until it terminates naturally). The dual-socket
polling is removed once legacy sessions are gone.

### Multi-session model

Unchanged at the session level: every worker and orchestrator keeps its own detached
tmux session. The attached client is a *view*, not the session:

- Clients exist only for on-screen sessions (focused view, or each pane in Split view);
  navigating away detaches. Typically 1–2 clients alive regardless of session count.
- Each session has at most one attached client (the browser route reads the tap, it
  does not attach), so `window-size latest` sizes each session to the panel showing it.
  No sizing crosstalk between sessions.
- Background sessions keep their `pipe-pane` FIFO taps feeding the event bus — the
  browser WebSocket route and activity/completion detection are untouched.
- Switching sessions = detach A, attach B; tmux full-repaints B into a fresh emulator.
  Navigation and restore are the same code path.

## Components

### 1. `AttachedClient` (new, `ninox-core`)

Owns a PTY pair and a child `tmux -L ninox attach -t <id>` process (spawned with
`TERM=xterm-256color`).

- `spawn(session_id, cols, rows) -> AttachedClient`
- `resize(cols, rows)` — `TIOCSWINSZ` on the PTY; tmux follows via `window-size latest`.
  Replaces explicit `resize-window` calls and the bounce-resize replay hack.
- `write(bytes)` — keyboard/mouse input straight to the PTY master. Replaces the
  `load-buffer`/`paste-buffer` path.
- Output: async stream of raw bytes (the tmux client's rendering).
- `detach()` / kill-on-drop so navigation cannot leak client processes.

### 2. Input encoder (app, replaces `key_to_terminal_bytes`)

Pure function `(key, modifiers, text, TermMode) -> Option<Vec<u8>>`, where `TermMode`
is read from the live alacritty `Term`:

- Honors `APP_CURSOR`, `APP_KEYPAD`, and the kitty-keyboard flags
  (`DISAMBIGUATE_ESC_CODES` etc.) which alacritty tracks when the inner app enables
  them through tmux. Shift+Enter → `CSI 13;2u` when disambiguation is active.
- Paste is wrapped in bracketed-paste markers when that mode is on.
- Mouse events are SGR-encoded when the inner app has requested mouse reporting;
  wheel scroll goes to the app when it wants it, to ninox scrollback otherwise.

### 3. Scrollback provider (new)

tmux pane history is the source of truth (`history-limit 100000`).

- On scroll above the live screen, fetch the needed range with
  `capture-pane -e -J -S <start> -E <end>`, parse the styled text into render-ready
  lines with a throwaway vte parser, and cache them.
- Live rendering is unaffected; "jump to latest" drops the history view.
- History survives reattach for free (tmux keeps it).
- Selection/copy works across cached history lines.

Deleted along with their tests: `extra_history`, `extra_offset`, `detect_shift`,
`capture_evicted_content`, and the per-`ESC[H` frame splitting in
`TerminalState::process`.

### 4. Renderer fidelity pass (`terminal.rs::draw`)

- Cell metrics measured from the actual monospace font at load time, replacing the
  `(0.6, 1.4) × font_size` approximation (single source of truth for canvas drawing,
  hit-testing, and PTY sizing).
- Color: truecolor + 256-color cube + theme-driven ANSI-16 palette (from the existing
  theme variants) instead of the hardcoded base 16.
- Cursor shape (block/underline/beam) and blink from DECSCUSR.
- Underline variants (single/double/curly), strikethrough, dim, reverse.
- Nerd Font PUA fallback kept as-is.

## Data flow (focused session)

```
keyboard/mouse → input encoder → AttachedClient PTY ─┐
                                                     ▼
                       tmux server (ninox socket; session grid + history)
                                                     │ client repaints
                                                     ▼
                       AttachedClient reader → alacritty Term → canvas draw
scroll-up → scrollback provider → capture-pane → styled line cache → canvas
```

Background sessions: `pipe-pane` tap → FIFO → event bus → WebSocket route / activity
detection (unchanged).

## Error handling

- **Attach fails / session gone:** mark the session `Terminated` (existing behavior).
- **Client process dies unexpectedly:** reader sees EOF → auto-reattach once; on
  repeated failure show the existing "Terminal connecting…" state with the error.
- **`capture-pane` failure while scrolling:** degrade to cached history, log a
  warning; never block the live view.
- **tmux too old:** startup check requires ≥ 3.2 and names the installed version.

## Testing

- **Input encoder:** table-driven unit tests over key × modifiers × TermMode —
  Enter vs Shift+Enter vs kitty mode, arrows in app-cursor mode, bracketed paste.
- **Scrollback provider:** fixture tests parsing real `capture-pane -e` output into
  styled lines (same style as existing `testdata/*.bin` tests).
- **Integration (gated on tmux availability, existing pattern):** spawn a session
  running a byte-echo script; attach; assert the attach repaint arrives, PTY resize
  causes tmux reflow, and Shift+Enter arrives as `CSI 13;2u`.
- **Renderer:** keep captured-PTY replay tests asserting against the emulator grid;
  add a metrics test that cell size comes from font measurement.
- **Manual end-to-end:** drive Claude Code in a session — multi-line input, scrolling
  during a long generation, app restart with scrollback intact.

## Out of scope

- Switching the browser route or background monitoring to attached clients.
- Replacing the Iced canvas with an external terminal widget.
- tmux control mode (`-C`) integration.
- OSC 4/10/11 dynamic palette queries beyond the theme-driven base 16.
