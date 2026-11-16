# ACP Tier 2: extend `AcpBackend` to codex and pi (MLOPS-5311)

**Status:** Implementation plan, committed alongside the code it describes.

**Builds on:** PR #225 (`AcpBackend` for OpenCode), PR #227 (`ninox-server`
connection ownership), both phase-1/OpenCode-only by their own explicit
scoping. See `docs/superpowers/specs/2026-10-21-cross-harness-messaging-design.md`'s
"Resolved decisions" section, decision 2, for the adapter choices this plan
implements.

## Scope

Per the design spec's resolved decision 2, of the four originally-named
Tier 2 candidates:

- **opencode** — native, already shipped (#225-227).
- **codex** — needs `@agentclientprotocol/codex-acp` (no version pinned by
  the spec; described as the most mature of the three adapter-dependent
  harnesses).
- **pi** — needs `pi-acp@0.0.34` pinned (the spec explicitly flags this as
  the adapter expected to need "the most implementation-time rework as it
  matures" — the least mature of the two being added here).
- **antigravity** — spec resolution: **defer**, not implemented. Every
  available adapter is an unofficial wrapper and several flag Google's ToS
  as treating third-party access as grounds for account suspension — a
  disqualifying risk independent of maturity, not something this PR
  revisits.

Per the ranking above, **codex is implemented before pi** in this change,
since pi is the adapter the spec itself flags as least mature.

Out of scope, matching #225/#227's own stated boundaries (not a gap this
PR introduces — true for opencode today too):

- Per-harness spawn-time routing (`ninox spawn` producing an ACP-backed
  session directly). `Backend::Acp` still isn't reachable from
  `Runtime::for_new`/`configured_backend` for any harness; `POST
  /:id/acp` remains the only way an ACP connection comes into existence.
- `worker_status::ActivityEvent` integration for live-activity chips (the
  design spec's resolved decision 3's correction) — unbuilt for opencode
  too, not part of this change.
- TUI/GUI differences between opencode/codex/pi transcripts — the existing
  generic `TranscriptEntry` rendering already covers any ACP agent
  uniformly; no harness-specific rendering is needed.

## Design

### `AcpBackend`: one fixed command → a per-harness command table

Today `AcpBackend` holds a single `agent_command: Vec<String>` baked in at
construction (`AcpBackend::new()` hardcodes `opencode acp`); `create_session`
discards its `cmd` argument entirely. Generalizing to three harnesses while
keeping `ninox-server`'s "one `AcpBackend` instance, one connection map"
model (same `Engine::acp: Arc<AcpBackend>` field, same HTTP routes, same
`knows`/`send_keys_detached` call sites — none of this changes) means
resolving the launch command **per session**, not per backend instance:

- `AcpBackend` gains `agent_commands: HashMap<String, Vec<String>>`
  (harness name → argv), built by `default_agent_commands()`:
  - `"opencode"` → `["opencode", "acp"]` (unchanged)
  - `"codex"` → `["npx", "-y", "@agentclientprotocol/codex-acp"]`
  - `"pi"` → `["npx", "-y", "pi-acp@0.0.34"]` (version pinned per the
    spec's resolved decision)
- `create_session`'s existing-but-previously-ignored `cmd: &str` parameter
  is repurposed as the **harness name** to look up in that table — the same
  kind of backend-specific reinterpretation of the generic `SessionBackend`
  contract this file's doc comments already describe for ACP mode. An
  empty string defaults to `"opencode"` (preserves every existing call
  site's exact behavior unmodified, since per-harness spawn routing still
  doesn't exist and today's only caller never set a harness).
- `AcpBackend::with_agent_command` (the full-override test seam) is
  unchanged in behavior — still ignores the harness argument entirely, for
  tests that only care about pointing at one fake agent.
- A new `AcpBackend::with_agent_commands(map)` constructor supports tests
  that need genuine per-harness dispatch (proving `create_session` actually
  launches a different command per harness name), without touching
  `ninox-server` or `Engine`.

This keeps the blast radius to `acp_backend.rs` plus the two call sites
that need a harness name threaded through: `CreateAcpSessionRequest`
(`ninox-server`) gains a required `harness` field, and
`messaging::is_acp_capable_harness` grows to recognize all three names.
`InProcessAcpRouter`/`HttpAcpRouter`/`Engine`'s teardown paths are
untouched — they all operate purely by session id, which was already
harness-agnostic.

### `messaging::is_acp_capable_harness`

Extended from `session.agent_type == "opencode"` to also match `"codex"`
and `"pi"` — this is the delivery-routing gate (Tier 2 vs. falling back to
Tier 3 keystrokes), unchanged in every other respect: still reads one
session column, still `false` for any session this process never
connected via `AcpRouter`.

### Why not route this through `harness.rs`'s `HarnessSpec`

`harness.rs` already existed during #225/#227 and phase 1 deliberately
didn't wire OpenCode's ACP command through it — `default_agent_command()`
and `is_acp_capable_harness` are both hardcoded independently of
`HarnessSpec`. This PR continues that same precedent rather than
introducing a new per-harness registry mid-stream: `HarnessSpec` is about
pty spawn argv (`interactive_args`/`worker_args`/`resume_args`), a
different axis than "which binary speaks ACP," and wiring `AcpBackend`
construction through `AppConfig`/`HarnessRegistry` would be a structural
change to `Engine::new()` far outside this ticket's scope.

## Testing

Mirrors #225's own pattern exactly, per harness:

- Unit tests on `default_agent_commands()`/`command_for` (the lookup table
  itself, including the empty-string-defaults-to-opencode and
  unknown-harness-errors-cleanly cases).
- A fake-agent duplex-pipe round trip per new harness (reusing the
  existing `run_fake_agent`/`wire_fake_session` helpers, which are already
  protocol-shaped rather than opencode-shaped).
- A `create_session`-level dispatch test (via `with_agent_commands`) proving
  the harness name actually selects a different real subprocess command,
  not just that the lookup table itself is right.
- `#[ignore]`d real-binary smoke tests for `codex-acp`/`pi-acp`, gated on
  `npx --no-install <pkg> --version` succeeding (never triggers a network
  fetch), mirroring the existing opencode smoke test's "skip, don't hang or
  fail" contract.
- `messaging.rs`: extend the existing capability-gate test to assert
  codex/pi are now ACP-capable, claude-code/aider still aren't, and add the
  existing `deliver_via_keystrokes` routing tests' coverage for the new
  harnesses (they're parametrized over `is_acp_capable_harness`, which
  needed no further duplication — the existing FakeAcpRouter tests already
  exercise the general "capable but no connection" / "capable with a
  failing connection" paths; only the capability predicate itself needed
  new harness coverage).
