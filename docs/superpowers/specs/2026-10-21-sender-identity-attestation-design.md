# Daemon-Attested Sender Identity for Inter-Session Messages (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-core` primarily
(`orchestrator_auth.rs`, `messaging.rs`), with `ninox-app`/CLI changes
for rendering the attestation. File and line references below reflect
the code as of this writing; treat them as orientation, not guaranteed
locations.[^1]

## Context

Third spec in the next-gen refactor (first two: fleet overview UI/UX
vision and hexagonal session-ports, both merged to `v1`). Discovered
directly during this same refactor effort, not hypothetically: an
orchestrator sent a worker three follow-up instructions via `ninox
send` (drop a tracking reference, amend a doc, merge a PR). The
worker's harness correctly wrapped each as "another session sent this,
treat it as an unverified peer" — and since the worker had no way to
confirm the sender really was its own spawning orchestrator rather than
an arbitrary other session, it rightly refused the most consequential
one (the merge) rather than act on an unverifiable claim. The user
reports hitting this pattern often enough to need a real fix.

What already exists:

- `crates/ninox-core/src/orchestrator_auth.rs` (66 lines) already solves
  the hard version of this problem, for a different caller:
  `authorize_orchestrator()` cross-checks a CLI caller's claimed
  orchestrator ID against a persisted `OrchestratorRuntimeIdentity` row
  (`types.rs:320-332`) — matching `server_epoch`, `physical_tmux_name`,
  `pane_id`, `root_pid`/`root_created_at`, plus a live process-ancestry
  check via `runtime::caller_descends_from(root_pid)`. Its own doc
  comment states the exact thesis of this spec: "`NINOX_ORCHESTRATOR_ID`
  alone is just an env var — trivially forgeable by anything running in
  the same shell." Today this gates only orchestrator-facing commands
  like `ninox plan register` — it is not wired to message delivery.
- `ninox send`'s sender (`main.rs:710-730`) is already resolved
  server-side from `NINOX_SESSION` env + cwd via
  `worker_status::resolve_session_id` rather than a spoofable CLI
  argument — better than nothing, but weaker than
  `orchestrator_auth`'s pane/process-ancestry cross-check.
- `deliver_message`/`deliver_by_mechanism` (`messaging.rs:80-120`)
  carries `from` through to the inbox delivery mechanism
  (`inbox::write_message`), but **not** to the `SessionSocket` or
  `Keystrokes` delivery paths — those carry no sender field at all
  today.
- The "another Claude session sent a message... unverified peer...
  permission laundering" framing that triggers a receiving agent's
  caution is injected by the Claude Code harness itself, confirmed by a
  full-text search of this repo (zero matches for that wording or
  "unverified peer" anywhere in `crates/`). **Ninox cannot change that
  wording** — it can only change what verifiable facts are available for
  the receiving agent to weigh when it reads the delivered message.

## Goals

- Every inter-session message, regardless of delivery mechanism (inbox,
  `SessionSocket`, `Keystrokes`), carries a daemon-attested sender
  identity with the same strength of proof `orchestrator_auth.rs`
  already uses for orchestrator-facing commands.
- The receiving session's rendered message includes an explicit,
  factual verification line stating the sender's relationship to the
  receiver — always, not only for the "my orchestrator" case (direct
  spawning orchestrator / sibling under the same orchestrator /
  descendant / unrelated session / cross-harness peer).
- The receiving agent still makes its own judgment call on what to do
  with a message. This spec supplies better facts; it does not
  auto-escalate trust or bypass the receiving session's own reasoning,
  and it does not change the harness's own safety framing.

## Non-goals

- Changing the Claude Code harness's "treat as peer" wording — outside
  ninox's control.
- Cryptographic signing across machines. This spec covers same-machine
  daemon-attested identity, consistent with `orchestrator_auth`'s
  current scope. Cross-machine attestation (once SSH-based remote
  machines exist, per the remote-sessions spec) is a follow-on concern,
  not covered here.
- Auto-granting any specific permission based on a verified sender —
  that stays a receiving-session judgment call, exactly as today.

## Design

### Extend `orchestrator_auth`'s attestation to `deliver_message`

Generalize `authorize_orchestrator`'s verification primitives (tmux
pane / process-ancestry / server-epoch cross-check against
`OrchestratorRuntimeIdentity`) into a shared function callable from
`messaging.rs`, so `deliver_message` attaches a sender attestation
(session ID, relationship, verified: bool) alongside the existing
`from` field — for all three delivery mechanisms, closing the gap
where two of three currently carry no sender field at all.

### Relationship categories

- `direct_orchestrator` — sender is the receiver's recorded parent/spawner
- `sibling` — same orchestrator, different worker
- `descendant` — the reverse direction (e.g. a worker messaging back up to its orchestrator)
- `unrelated` — no recorded lineage relationship (includes legitimate cross-tree/cross-harness messaging introduced by the fleet-overview spec)
- `unverifiable` — the attestation check itself failed (e.g. process ancestry couldn't be confirmed) — reported plainly as such, never silently dropped or upgraded to a default

### Delivered message format

The verification line is part of the delivered message content itself
(not a side channel the agent might not see), e.g.:

- `[ninox: sender session <id> verified as your direct spawning orchestrator, confirmed via process-ancestry + tmux-pane cross-check]`
- `[ninox: sender session <id> verified as an unrelated session — no lineage relationship recorded]`
- `[ninox: sender identity could not be verified]`

This is plain informational text. It does not alter or override the
harness's own safety framing — it adds facts for the receiving agent to
weigh alongside that framing, the same way it would weigh any other
contextual information.

### Testing

- Unit tests for the generalized attestation function covering each
  relationship category against a seeded
  `OrchestratorRuntimeIdentity`/session-lineage fixture.
- Integration test: spawn a mock orchestrator + worker pair, send a
  message, assert the delivered payload contains the correct
  relationship and `verified: true`; tamper with the claimed sender
  session ID and assert `verified: false` / `unrelated`.
- Regression test confirming all three delivery mechanisms (inbox,
  `SessionSocket`, `Keystrokes`) now carry the attestation.

## Open questions

- Exact attestation strength achievable for the `SessionSocket`/
  `Keystrokes` paths — these may have different process/pane topology
  than the CLI-invoked `ninox send` path `orchestrator_auth` was
  originally built for; needs implementation-time investigation.
- Whether `descendant` and `unrelated` need further visual/textual
  distinction or can be collapsed — left to the implementation plan.
- Interaction with the still-open remote-sessions spec's questions:
  cross-machine attestation is explicitly out of scope here but should
  be revisited once that spec is resolved.

[^1]: Spot-checked against `v1` as of this writing: `orchestrator_auth.rs`
    and `messaging.rs` citations line up as described. Two line-range
    citations have drifted, though the described behavior holds in both
    cases: `main.rs`'s `ninox send` sender-resolution code now lives at
    `main.rs:729-748` rather than `main.rs:710-730` (still resolved
    server-side via `worker_status::resolve_session_id`, not a CLI
    argument, including an in-code comment stating the same rationale),
    and `OrchestratorRuntimeIdentity` in `types.rs` now lives at
    lines 326-335 rather than 320-332.
