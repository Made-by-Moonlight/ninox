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
