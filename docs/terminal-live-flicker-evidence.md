# Steady-state live terminal flicker evidence

Captured on macOS with tmux 3.7b from base
`004f71c248f6810d571c67036058d4b7ef297d8f`. The reproduction used an
isolated tmux socket and a real hidden PTY client; it did not attach to the
running Ninox app or its server.

An inner application opened DEC 2026, erased the screen, wrote the replacement
over two delayed pane reads, then closed DEC 2026. The relevant hidden-client
reads were:

```text
\x1b[?2026h\x1b[J\x1b[?2026l
\x1b[?2026h\x1b[?25l\x1b[?12l\x1b[?25h\x1b[9;7H\x1b[?2026l
\x1b[?2026h\x1b[?25l\x1b[Hfirst rows...\x1b[9;1Hbottom-final...\x1b[?2026l
```

The complete capture had four `CSI ?2026h` and four `CSI ?2026l` markers.
Server `source-file` reconciliation had applied `xterm*:...:sync`, and a new
hidden client on the preserved server received balanced synchronized frames.
Framing was therefore present and balanced.

The first balanced outer frame was nevertheless only an intermediate erase.
tmux had split one inner synchronized update into three outer commits: erase,
cursor-only state, then content repaint. Alacritty correctly committed each
balanced outer frame. Ninox then cleared the iced canvas cache for every PTY
event, so the blank grid and transient cursor became paintable states. RTL
visual mapping happened after that commit and did not create the transient.
The 8 ms coalescer bound could not cover the observed 80 ms pane-read gaps.

The deterministic replay starts from an already hydrated stable grid and sends
only steady-state live bytes. No capture range, history replay, paging, or
initial attachment behavior participates.

Regression assertions cover terminal state and renderer commit count. The
erase and cursor-only reads cause zero commits; the final content frame causes
one. Incomplete frames recover after tmux's one-second synchronization bound as
one synthetic atomic close, while ordinary unframed echo/log output commits
without that recovery delay.

## Follow-up live capture

Captured from the installed and running `6caa0e5e692e02f63b87b6c52f4c17a77a7c5db0`
bundle. Its executable SHA-256 was
`f0bee8d0444f5de930c736833d05ed356941986f5c9bbbe787eec369dc069fd2`.
The attached `xterm-256color` client reported `sync` in its effective features,
and the managed config contained `xterm*:RGB:usstyle:extkeys:hyperlinks:sync`.

An already hydrated Cursor terminal showed a different steady-state boundary
than the earlier split-frame capture:

- Pane-side output contained no DEC 2026 markers.
- One bottom-region repaint arrived as 1.5–1.8 KiB over two PTY reads within
  0.6 ms. The 3 ms quiet / 8 ms hard-cap coalescer correctly made it one event.
- The event contained 7–10 erase-line commands plus the complete replacement
  text, so alacritty advanced to one final grid and Ninox invalidated the iced
  canvas once.
- 10–190 ms later, tmux repeatedly emitted a separate exact 16-byte
  `\x1b[?2026h\x1b[?2026l` frame with no payload.
- `TerminalOutputFramer` forwarded that empty frame. Alacritty made no visible
  grid mutation, but `commit_output` still cleared the whole canvas cache and
  scheduled a second identical draw.

No cursor visibility sequences occurred in these events, and ScreenCaptureKit
sampling found no independent blank compositor frame. The remaining redundant
paint therefore began at Ninox's no-op frame handling, after tmux framing and
PTY/event batching but before iced cache invalidation. The deterministic
regression replays the observed two-event topology and requires one renderer
commit, not two.
