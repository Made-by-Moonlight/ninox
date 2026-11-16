# Ninox Prompt Agent Implementation Plan

**Goal:** back the "Ask ninox" prompt with a dedicated, long-running worker
type — the **prompt agent** — instead of routing each prompt submission to
whichever orchestrator happens to be selected (the MLOPS-5312/5319 behavior
this supersedes for the v1 shell).

**Ticket:** MLOPS-5415. **Brief:**
`.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5415-prompt-agent.md`
(read `BRIEF-common.md` §7 first).

**Scope of this PR:** core + CLI only (`feat(core)` per the PR title the
orchestrator assigned). UI call-site wiring (prompt bar submit, reply strip,
target chip, clear button/hotkey) is explicitly left to the GUI/TUI try-out
workers (`v1-mlops-5339-tryout-fixes`, `v1-mlops-5350-tryout-fixes`) building
on the core API's signatures, per the brief's item 7.

## Design

### Worker type

A new built-in `WorkerTypeSpec` named `prompt` (`ninox_core::worker_type::
PROMPT_WORKER_TYPE`), registered in `builtin_specs()` alongside
Implementation/Researcher/Evaluator. Its `brief_template` is the agent's role
brief, ending in the required `{prompt}` placeholder — composed once, via
the existing `WorkerTypeSpec::compose_prompt`, with the *first* question a
freshly spawned (or freshly-fresh-restarted) agent receives. Every later
`ask` delivers its text unmodified, like `ninox send`, since the role is
already established in the live conversation.

Its `WorkerPermissionsConfig` is populated with the brief's allow/deny
rules as data (ninox-wide `allow`/`deny`/`ask` permission-rule enforcement
itself is still unimplemented anywhere in this codebase — see
`worker_type.rs`'s own doc comment — so this is forward-looking, same as
every other worker type's profile today).

**Known gap, deferred to MLOPS-5416** (`v1-mlops-5416-worker-types`,
building `ninox worker-type create/edit/show/list` and a hidden/system-type
flag in parallel): `prompt` is not yet marked hidden, so it will appear like
any other builtin in a plain worker-type listing (e.g. the TUI's Worker
Types screen) until that flag lands. Noted in the PR description as a
follow-up, not fixed here to avoid duplicating 5416's work.

### Lifecycle: a fixed, per-machine singleton id

Rather than searching the store for "a session with `worker_type ==
'prompt'`," the singleton uses a fixed, deterministic id
(`prompt_agent::PROMPT_SESSION_ID = "ninox-prompt"`) — existence of that row
*is* the lifecycle state, with no lookup needed. (Only the local machine is
handled; a per-remote-machine prompt agent is out of this PR's scope — the
brief's "one prompt-agent session per machine" is read as "one, on the
machine running this `ninox` process," not one per configured remote.)

`ask(store, config, text)`:
1. No row yet → `spawn_fresh`: resolve the `prompt` worker type, create its
   scratch workspace (`AppConfig::prompt_agent_workspace_dir()` — a plain
   directory, never a git worktree, since the type's permission profile
   allows no file edits outside it), launch via the harness's
   `interactive_cmd` (never `worker_cmd`'s `--dangerously-skip-permissions`
   — this is a long-running, permission-respecting session, architecturally
   closer to an orchestrator than a one-shot `ninox spawn --type` task),
   wait for its input prompt, then deliver the composed role-brief+question
   as its first message.
2. Row exists, pane alive → deliver `text` directly (`ninox send`'s own
   `messaging::deliver_message`).
3. Row exists, pane dead → restart transparently, preferring `--resume`
   with the stored `claude_session_id` (conversation survives a crash) and
   falling back to a fresh restart only when the harness can't resume; a
   fresh restart re-delivers the composed role-brief+question (the
   conversation is gone, so the role framing must be re-established), a
   resumed one just delivers `text`.

`clear(store, config)`: `/clear` (a literal, unframed keystroke via
`runtime::send_keys` — not a `ninox send` message) for a live claude-code
pane, since that resets conversation state without restarting the process.
Any other harness, or a currently-dead pane, falls back to the same
deliberate-fresh-restart path `ask`'s case 3 uses. This is the only path
that discards the conversation on purpose — `ask`'s crash-recovery restart
always prefers resume.

`reply_since(cursor)`: the agent's pane text since a previous byte-offset
cursor, via the same `runtime::read_screen` `ninox read` itself uses.

### Fleet/island/Board exclusion

`fleet::snapshot::FleetSnapshot::load` — the single upstream source for the
v1 Fleet tab's island model (`ninox-overview`) and (once built) the
restyled Board — skips any session whose `worker_type ==
Some(PROMPT_WORKER_TYPE)` before it ever becomes a `FleetMember`. It is
unaffected everywhere else: `ninox list`/`ninox connect` (which read the
store directly, not through `FleetSnapshot`) still reach it, satisfying the
brief's "must be visible somewhere ... and the user can connect to its
pane."

### CLI

`ninox prompt ask "<text>"` / `clear` / `status [--json]`, dispatched from
`cli.rs` alongside `Spawn`/`Send` (not the hot-path short-circuit group —
like `Spawn`, a first `ask` may need the same tmux-config/wrapper-hook setup
a fresh interactive launch depends on). Each subcommand is a thin wrapper
over `ninox_ui_shared::prompt_agent`'s `ask`/`clear`/`status` — the same
functions the GUI and TUI call directly, never shelling out to this binary.

## Testing

Every `prompt_agent` function is split into a thin public wrapper
(hardcoding the real singleton id and `AppConfig` path helpers) and an
`*_impl` twin taking the session id and filesystem paths explicitly, so
tests exercise the real code path under a unique per-test id against a real
tmux/ptyd pane (mirroring `restart.rs`'s own end-to-end test pattern — a
`sh -c` stand-in harness that draws a `❯` prompt and echoes input) without
ever touching a real `ninox-prompt` session or the real `~/.config/ninox/`
paths.

Covered: worker-type registration (builtin presence, deny rules present),
singleton lifecycle (spawn on first ask, reuse without respawning or
re-delivering the role brief, transparent restart preferring `--resume`),
fleet-snapshot exclusion, `ask` delivering text, `clear` sending `/clear`
without restarting the process.

## Deviations / follow-ups

- `prompt`'s hidden/system flag and `can_spawn: none` — MLOPS-5416's
  addition; noted above.
- UI wiring (prompt bar call sites, reply strip, target chip, clear
  button/hotkey) — left to the GUI/TUI try-out workers per the brief.
- Remote-machine prompt agents — out of scope; only the local machine is
  handled today.
