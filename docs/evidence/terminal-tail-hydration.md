# Terminal tail hydration evidence

Base: `004f71c248f6810d571c67036058d4b7ef297d8f`

Capture used an isolated private tmux socket and a synthetic 2,000-row pane. It
did not attach to a Ninox or user tmux server.

| scenario | bounded tail | attached PTY | observed replay | sync frames |
| --- | ---: | ---: | ---: | ---: |
| same-size attach | 299 bytes / 24 rows | 1,538 bytes | rows 1979–2000 | 3 |
| attached resize, unframed pane reflow | 299 bytes / 24 rows | 31,980 bytes | 1,228 rows (7–2000) | 476 |
| attached resize, mode-2026 pane reflow | 299 bytes / 24 rows | 1,538 bytes | rows 1979–2000 | 3 |
| steady writes, 20ms apart | n/a | 675 bytes | states 1–5 | 3 |

The attach redraw itself is tail-bounded. The visible full-chat replay comes
from resizing the pane after the client is attached: the TUI reflows historical
content as live pane output, and tmux exposes many individually atomic redraws.
Advertising tmux `sync` does not combine an unframed producer's whole reflow
into one frame.

The implementation now resizes while detached, waits for two identical bounded
tail captures, paints the latest tail once, and attaches at that exact size.
Fragmented attach mode-2026 framing is gated through one complete frame. The
steady-state capture remains atomic per frame, but tmux emits multiple frames
for producer updates beyond the coalescer's 8ms hard cap. Tail hydration cannot
merge those logically separate steady-state frames; that residual cause is
independent and remains outside this change.

Deterministic regressions live in:

- `crates/ninox-core/src/client.rs` (`initial_output_gate_*`)
- `crates/ninox-core/src/tmux.rs` (`viewport_capture_*`)
- `crates/ninox-app/src/components/scrollback.rs` (`deterministic_fake_tmux_*`)
- `crates/ninox-app/src/components/terminal.rs` (`synchronized_fragments_*`)
