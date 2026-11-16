# Cross-Harness Messaging and Execution (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-core` (`messaging.rs`, `harness.rs`,
`runtime/`). File and line references below reflect the code as of this
writing; treat them as orientation, not guaranteed locations.

## Context

Item 8 of the next-gen refactor. The fleet overview UI/UX vision spec
(item 7, merged) already covers how cross-harness messages are
*displayed* (the same edge-flash mechanism as any other message) and
how live tool-call activity renders on a node (a chip, e.g. "⚒ bash
×3"). This spec covers how a message or structured update actually
reaches/comes from a session running a different harness than the
sender — previously undesigned.

### What exists today

- `crates/ninox-core/src/harness.rs`'s `HarnessSpec` (registry covering
  claude-code/codex/opencode/aider/freebuff) has fields only for
  spawn-time CLI argv (`binary`, `model`, `interactive_args`,
  `worker_args`, `resume_args`, `known_models`, `models_cmd`) — nothing
  about runtime communication capability.
- `messaging.rs`'s `deliver_by_mechanism` has three paths. Two are
  hard-gated to claude-code specifically:
  - `SendMechanism::SessionSocket` → `session_socket::find_peer`, whose
    own doc comment states this is "Claude Code's own cross-session
    messaging socket... the transport its `SendMessage` tool uses" — a
    Claude-Code-internal protocol ninox speaks, not one it owns.
  - `SendMechanism::Inbox` → `target_can_drain_inbox` explicitly checks
    `session.agent_type != "claude-code"` and returns `false` otherwise;
    codex/aider/opencode "never get `.claude/settings.local.json` hooks
    processed by anything" (its own doc comment).
  - `SendMechanism::Keystrokes` (`runtime::send_keys`) is the only
    universal path today: typing characters into the pty via
    `SessionBackend`, which has no harness parameter anywhere.
- The sender-identity attestation spec (item 10, merged) only
  acknowledges cross-harness messages exist as an attestation category
  (`unrelated`), without addressing delivery mechanics.

### What was discovered via live research (new, confirmed via web, not this repo)

The **Agent Client Protocol (ACP)** — [agentclientprotocol.com](https://agentclientprotocol.com) —
is a real, open, JSON-RPC-over-stdio standard for editor/client ↔
coding-agent communication, originated for Zed but implemented far
beyond it. Per Zed's own agent directory
([zed.dev/acp](https://zed.dev/acp)):

- **Native ACP support**, confirmed: Claude Agent, **Codex CLI**, Gemini
  CLI, Cline, Cursor, Devin, GitHub Copilot, Goose, OpenHands, Poolside,
  Zed Agent.
- **Via adapter**, confirmed: **Google Antigravity**, **OpenCode**, **Pi**,
  and dozens of others.
- OpenCode additionally documents this directly on its own site
  ([opencode.ai/v2/docs/cli/acp](https://opencode.ai/v2/docs/cli/acp/)):
  running `opencode acp` starts it as an ACP agent, exchanging
  newline-delimited JSON-RPC over stdin/stdout — not a pty, not
  keystroke injection.
- Pi has at least two independent community ACP adapters
  (`pi-acp`, `pi-acpinator`), both wrapping `pi --mode rpc` and speaking
  ACP JSON-RPC over stdio to the adapter's own stdin/stdout.

**Correction (resolution pass, see "Resolved decisions" below):** live
re-verification found the native/adapter split above backwards for two of
these four harnesses. **Codex CLI does not have native ACP support** — it
needs `codex-acp`, an adapter Zed built and open-sources, the same shape of
dependency as antigravity/pi's adapters. **OpenCode does** have native
support (line 57-61 above already said as much, inconsistently with its own
"via adapter" listing at line 55) — `opencode acp` speaks ACP directly, no
adapter process. Of the four, only opencode is genuinely native.

This means **every one of this project's four originally-named target
harnesses, plus opencode (already in ninox's own registry), has a real,
working path to structured async communication** — this is not
greenfield protocol design, it's adopting an existing, increasingly
cross-vendor-adopted standard.

The catch: ACP support is typically an *alternative launch mode*
(`opencode acp`, `pi --mode rpc` via an adapter), not a side-channel
layered on top of normal interactive pty use. A session run this way
communicates entirely via structured JSON-RPC — there is no raw
terminal output to display or attach to in the traditional sense.

## Goals

- For any harness with confirmed ACP support, ninox communicates via
  ACP as the session's actual execution/interaction protocol — real
  async messaging, streamed structured updates, and tool-call events
  that feed directly into item 7's existing live-activity node design
  — not merely "messaging" bolted onto an otherwise-unchanged pty
  session.
- Claude-code sessions keep their existing, already-working mechanisms
  (pty + inbox/hook + session-socket) **unchanged** — ACP-native mode is
  not a forced migration for the one harness that already has a good,
  purpose-built solution.
- Any harness without confirmed ACP support (aider, and any future
  unknown harness) falls back to a universal, framed, idle-timed
  keystroke-delivery baseline — no harness is ever left with undefined
  or silently-failing messaging.

## Non-goals

- Migrating claude-code off its existing inbox/session-socket/pty
  mechanisms.
- Inventing a new, ninox-owned IPC protocol — ACP is real and adopted
  elsewhere; this spec reuses it rather than building a parallel
  standard.
- Pinning a specific ACP protocol version or a specific third-party
  adapter package/version for antigravity or pi — left to the
  implementation plan (see Open questions); these are external,
  not-ninox-owned dependencies of varying maturity.

## Design

### Three-tier delivery/execution model

1. **Claude-code** — unchanged. Existing inbox+hook and session-socket
   mechanisms, with pty-keystroke as their own existing fallback.
2. **ACP-confirmed harnesses** (codex, opencode, antigravity, pi — native
   or via a maintained adapter) — a new `AcpBackend` implements the
   existing `SessionBackend` trait (`runtime/mod.rs:105`), following
   the same per-kind-impl pattern as `tmux_backend.rs`/`ptyd_backend.rs`
   (item 6's hexagonal-ports template). Spawning launches the harness in
   its ACP mode (directly, or via its adapter subprocess) instead of
   inside a pty; all communication is JSON-RPC over that subprocess's
   stdin/stdout. Sending a message becomes a native ACP prompt/turn
   call, not keystroke injection.
3. **Everything else** (aider, any harness without confirmed ACP
   support) — the universal framed, idle-timed keystroke-delivery
   baseline: message text wrapped in a clear delimiter block with the
   item-10 attestation line, deferred until the target's tracked status
   (`worker_status.rs`) is not `working`, then typed via the existing
   `SessionBackend::send_keys` path. This tier is the guaranteed
   fallback — no harness is ever undefined.

### Connection ownership: `ninox-server`, not the CLI

Unlike a tmux pane (re-attachable by any process via tmux's own server)
or a ptyd unix-socket pty, an ACP connection is a live, stateful
JSON-RPC pipe over a single subprocess's stdin/stdout — only one
process can hold it at a time; it cannot be casually re-attached from
multiple callers. This means `ninox-server` (the persistent daemon, not
whatever short-lived CLI process happens to invoke `ninox send` or
similar) must be the one that actually spawns and holds each
`AcpBackend` connection, for the lifetime of that session.

The CLI's existing pattern of talking to `ninox-server` over its
HTTP/WS API, rather than doing anything to a session directly itself,
extends naturally here: a CLI-invoked "send a message" request becomes
an API call to the server, which looks up the target session and, if
it's ACP-native, writes the prompt over the connection it is already
holding. If the server isn't running, or doesn't have that session's
connection for some reason, this is a real error to surface — not
something to paper over by silently falling back to the keystroke
baseline. That fallback is reserved for harnesses with no ACP support
at all, not for "the connection to an ACP session happened to be
unavailable right now."

### Rendering implications for ACP-native sessions

An ACP-native session has no raw pty/terminal to display or attach to.
The TUI/GUI session-detail view (item 7's design) renders the live
structured transcript instead: ACP tool-call events map directly onto
the already-designed live-activity chip (e.g. "⚒ bash ×3"), and text
turns render as transcript entries. **Interjecting manually means using
ninox's own prompt/message controls** (already part of item 7's design)
for that session, not attaching to a literal terminal. This is a real
interaction-model change for these sessions and should be surfaced to
users explicitly (e.g. a visible "ACP-native — no raw terminal"
indicator), not silently different from a pty-based session that
happens to look the same until someone tries to attach.

### Adapter dependency, treated as external

Antigravity and pi need a third-party ACP adapter subprocess rather
than exposing ACP themselves (e.g. `pi-acpinator` wraps `pi --mode
rpc`). This spec deliberately does not pin a specific adapter
implementation or version — the implementation plan should select and
vet one per harness, treating it the same way the remote-sessions spec
treats SSH: an external transport ninox depends on but does not own or
maintain.

### `SessionBackend` integration

`AcpBackend` implements the existing trait. Its exact method mapping —
e.g. how a pty-shaped "read the current screen" method accommodates a
non-terminal, structured event stream — is left to the implementation
plan. The trait's current shape was designed around pty-based backends
and likely needs a small, additive extension (e.g. a transcript/event
accessor alongside the existing screen-read method) rather than a
forced-fit reinterpretation.

## Testing

- A mock ACP agent (a minimal stub speaking the protocol) for
  integration tests of `AcpBackend`'s spawn/message/receive flow,
  independent of any real external harness binary being present in CI.
- An explicit test that a harness without confirmed ACP support falls
  back to the universal keystroke baseline, and that claude-code
  sessions are provably unaffected by this spec (still use their
  existing mechanisms end to end).

## Resolved decisions (originally open questions)

This is the dedicated spec-resolution pass for `AcpBackend`, the same kind
this project has run for items 4, 6, 10 and 11 (see e.g. the remote-sessions
design's own "Resolved decisions" section) before implementation is spawned.
All research below was re-verified live against agentclientprotocol.com,
zed.dev/acp, and opencode.ai — not carried over from this spec's original
research — and one of the four tier-2 harnesses' support claims did not hold
up under that re-check (decision 2).

1. **Exact ACP protocol version to target.**

   **Resolved:** target `protocolVersion: 1`, the integer exchanged in the
   ACP `initialize` handshake's `protocolVersion` field (sent by the client
   and echoed/negotiated by the agent). Per
   [agentclientprotocol.com](https://agentclientprotocol.com)'s own
   initialization docs, this is "a single integer that identifies a MAJOR
   protocol version" that changes only for breaking changes, and it has been
   `1` since the protocol's inception — a different axis from the JSON
   schema's own finer-grained semver (a third-party survey puts the schema at
   `1.24.1` as of September 2026, tracking incremental, backward-compatible
   method stabilizations such as `session/close` and session-resume per
   ACP's own updates page). Those schema increments are refinements within
   `protocolVersion: 1`, not a version `AcpBackend` needs to negotiate
   around. The "full support for remote agents is a work in progress"
   caveat this spec originally cited is still live on agentclientprotocol.com
   today, but it is scoped specifically to *remote* agent deployment — a
   transport concern this design's local-subprocess-over-stdio model never
   touches. Pinning `protocolVersion: 1` now, rather than deferring to
   implementation time, is accurate, not premature.

2. **Which specific third-party adapter package to depend on for antigravity
   and pi.**

   **Resolved, with one amendment to this spec's premise:** live
   re-verification found that **Codex CLI's "native ACP support" claim in
   this spec's Context section does not hold.** Codex does not speak ACP
   itself — it requires the same kind of adapter-subprocess bridge as
   antigravity and pi, just one Zed built and open-sourced. Zed's own
   October 2025 "Codex is Live in Zed" launch post and the adapter's README
   confirm this: `codex-acp` is a Rust binary wrapping the Codex App Server,
   run in non-PTY mode specifically to avoid agent deadlocks, originally
   published as `@zed-industries/codex-acp` and now consolidated under
   `@agentclientprotocol/codex-acp`. Of this spec's four named harnesses,
   only **opencode** has genuine first-party/native ACP support — confirmed
   directly via its own docs at
   [opencode.ai/v2/docs/cli/acp](https://opencode.ai/v2/docs/cli/acp/):
   `opencode acp` speaks ACP protocol version 1 natively over stdio, no
   adapter process, no network port. Codex moves into the "via adapter"
   bucket alongside antigravity and pi.

   Per-harness recommendation for Tier 2:
   - **codex → `@agentclientprotocol/codex-acp`.** Built and open-sourced by
     Zed as Codex's own production ACP path inside Zed itself; the most
     mature and widely-used of the three adapter-dependent harnesses despite
     not being OpenAI's own code. (The predecessor `@zed-industries/codex-acp`
     package name still works but the README directs new installs to the
     `@agentclientprotocol` org.)
   - **opencode → no adapter.** Depend on `opencode acp` directly; this is
     the harness with the least external-dependency risk of the four.
   - **pi → `pi-acp` (`svkozak`/npm `pi-acp`), pinned at `0.0.34`.** This is
     the exact build Zed's own agent directory
     ([zed.dev/acp/agent/pi](https://zed.dev/acp/agent/pi)) documents and
     installs (`npx pi-acp@0.0.34`) — the strongest available "the ecosystem
     is consolidating behind this one" signal, ahead of same- or
     similarly-named competing forks (an MVP-stage `pi-acp` from `DxTa`, an
     SDK-embedding `pi-acp` from `victor-software-house`) and the
     alternative `pi-acpinator` (a smaller, faster-starting Rust
     reimplementation with no comparable ecosystem endorsement). 742 stars,
     README references current pi/Zed versions, but the README itself calls
     it an "MVP-style adapter," warns to "expect some minor breaking
     changes," and notes "some ACP features may not be implemented or are
     not supported." Pin the exact version rather than tracking `latest`,
     and expect this adapter — of the three Tier 2 adopts — to need the most
     implementation-time rework as it matures.
   - **antigravity → defer.** No official Google ACP support exists today;
     every available adapter (`agy-agent-acp`, `agy-acp-bridge`,
     `antigravity-acp`, and others) is an unofficial community wrapper
     around Antigravity's private `agy` CLI and its local conversation
     database, and several of their own READMEs flag that Google's terms of
     service characterize third-party access to Antigravity as a violation
     that "may lead to account suspension." That is a disqualifying risk
     independent of code maturity — don't force this harness into Tier 2 to
     complete the original four-harness set. Antigravity sessions stay on
     the universal keystroke-fallback tier (Tier 3) until Google ships or
     sanctions an adapter; revisit this decision if that changes.

3. **Exact `SessionBackend` trait extension needed.**

   **Resolved:** most of `SessionBackend` (`runtime/mod.rs:105-174`) already
   fits `AcpBackend` without any signature change — only one small, additive
   method is needed, and the live-activity-chip concern the spec's Design
   section attributed to this trait turns out to belong to a different,
   already-decoupled surface (see the correction at the end of this
   decision).

   **No trait change needed for:**
   - `create_session`/`kill_session`/`has_session`/`list_sessions`/
     `session_pid` — ordinary subprocess bookkeeping around the ACP agent
     (or adapter) child process; no pty-specific assumption in these
     signatures.
   - `send_keys` — maps directly onto an ACP `session/prompt` turn. This is
     already the "send a message" path the rest of the codebase calls; no
     new method needed for outbound messages.
   - `current_pane_identity` — `PaneIdentity` (`pub type PaneIdentity =
     crate::tmux::TmuxPaneIdentity`, `runtime/mod.rs:102`) is already
     backend-agnostic in practice despite its name: `PtydBackend`
     (`ptyd_backend.rs:505-514`) populates the same struct with synthetic,
     non-tmux values (its own host pid/epoch) rather than real tmux fields.
     `AcpBackend` does the same — e.g. `physical_tmux_name: "acp:<id>"`,
     `pane_pid` from the agent/adapter subprocess.
   - `read_screen`/`capture_history`/`history_size`/`attach_args` — these
     keep returning plain text/argv/counts; `AcpBackend` implements them by
     rendering its own structured transcript (see below) down to text, same
     as today's contract, just a different implementation body. `attach_args`
     returns argv for a read-only transcript follower rather than a tmux
     attach, consistent with this spec's own "Rendering implications"
     section (no raw terminal to attach to; interjection goes through
     ninox's own prompt controls instead).
   - `write_input` — forwards bytes as UTF-8 text through the same path as
     `send_keys`, with one special case: a literal `0x03` (Ctrl-C) byte maps
     to ACP's `session/cancel` notification rather than being forwarded as
     text, since ACP has a real cancel RPC where pty backends only have a
     raw interrupt byte.
   - `wake_idle_session` — a true no-op (`Ok(())`) for `AcpBackend`; ACP has
     no pty-idle-nudge concept to replicate.
   - `wait_for_input_prompt` — implemented using the ACP session's actual
     turn-idle state (an explicit signal ACP gives us), which is more
     reliable than the pty backends' screen-scrape heuristic. A genuine
     improvement, not a compromise.

   **One new method, with a default body (same non-breaking pattern as
   `answering`/`ensure_answering` at `runtime/mod.rs:165-173`, and the same
   "added alongside" precedent `SessionReactions` set in PR #213):**

   ```rust
   /// Structured transcript entries for backends with no raw terminal
   /// (e.g. `AcpBackend`) — the non-pty counterpart to `capture_history`.
   /// Pty-based backends have nothing structured to return.
   async fn transcript_history(&self, _id: &str) -> Vec<TranscriptEntry> {
       Vec::new()
   }
   ```

   `TranscriptEntry` is a small new enum (living beside `LiveSession`/
   `Liveness` in `runtime/mod.rs`) covering what an ACP `session/update`
   notification can carry: `UserMessage(String)`, `AgentText(String)`,
   `ToolCall { id: String, name: String, status: ToolCallStatus, detail:
   String }`, `TurnStarted`, `TurnEnded`. `TmuxBackend`/`PtydBackend` take
   the default and never override it — zero code change, zero behavior
   change, exactly the "unaffected by default" shape this question asked
   for.

   For *live* transcript updates (not just history-on-load), `AcpBackend`'s
   `start_streaming` implementation emits a new `Event::SessionTranscript {
   session_id: SessionId, entry: TranscriptEntry }` variant alongside the
   existing `Event::TerminalOutput` (`events.rs:12`), rather than trying to
   force structured updates through the byte-oriented `TerminalOutput`
   shape. This is an additive enum variant on `Event`, not a `SessionBackend`
   signature change; `TmuxBackend`/`PtydBackend`'s `start_streaming` keep
   emitting `TerminalOutput` exactly as today.

   Finally, `Backend` (`runtime/mod.rs:44-76`) gains a third variant,
   `Backend::Acp`, following the exact pattern `Backend::Ptyd` already set
   alongside `Backend::Tmux` (`ALL`, `description()`, `Display` each get one
   more mechanical arm).

   **Correction to this spec's Design section:** the live-activity chip
   (e.g. "⚒ bash ×3") that the Design section says should receive "tool-call
   events" from `AcpBackend` is **not** fed by `SessionBackend` or `Event`
   at all today. It's fed by `worker_status::apply_activity`
   (`worker_status.rs:136-205`), a plain `Store`-level function invoked only
   by Claude Code's own `PreToolUse`/`PermissionRequest`/`Stop` hooks via the
   `ninox worker-status` CLI verb — which is precisely why codex/opencode/
   pi sessions have *no* activity-chip data today (this spec's own Context
   section already notes they "never get `.claude/settings.local.json`
   hooks processed by anything"). The correct integration point for
   `AcpBackend` is therefore `worker_status::ActivityEvent`, not
   `SessionBackend`: `ninox-server`'s `AcpBackend` connection-handling loop
   (which already owns the Store and the live connection in the same
   process, per this spec's "Connection ownership" section) calls
   `apply_activity` directly off incoming `session/update` notifications,
   via new `ActivityEvent` variants analogous to the existing `HookPrompt`/
   `HookStop`/`HookAwaitingPermission` ones. This needs zero `SessionBackend`
   trait change and is a smaller, more targeted addition than the Design
   section implied — the chip path and the transcript/display path
   (resolved above) are two independent surfaces, not one trait's problem.

4. **Whether a user can force "raw/pty mode" for an otherwise ACP-capable
   harness.**

   **Not resolved in this pass.** This question is independent of the
   protocol version, adapter selection, and trait shape resolved above, and
   doesn't block starting `AcpBackend` implementation — it's a product
   decision about whether to offer an escape hatch back to Tier 3's
   keystroke-delivery model for users who prefer it. Left open for a future
   pass or for the implementation plan to raise explicitly if it turns out
   to matter sooner.
