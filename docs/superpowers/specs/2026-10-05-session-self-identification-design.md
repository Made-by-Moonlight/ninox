# Session Self-Identification (`whoami` / Role Rehydration)

**Status:** Proposed — spec only, no implementation in this change.

**Implementation target:** this repository, incrementally, as described in
Phasing below. File and function references point at code as of this
writing; later phases should re-check them before building.

## Context

[Moxy](https://github.com/Synthesia-Technologies/moxy), ninox's spiritual
successor, lets a session determine its own identity on demand instead of
relying on identity injected once at spawn time. Its daemon owns one
canonical, bounded role descriptor per session (profile, role, ownership,
lifecycle, authority, workspace, capabilities, boundaries, usable commands).
A `moxy whoami` CLI command and an OpenCode request-time hook project those
exact facts, resolved by the session's immutable ID — never inferred from
env labels, directories, panes, or model claims — and rebuilt fresh on every
request rather than cached, so correctness never depends on a compaction
checkpoint surviving (`docs/design.md` "Workers and Roles", Moxy repo).
Moxy's hook crosses a process boundary (OpenCode plugin → daemon) over an
authenticated loopback HTTP call using a domain-separated signing token,
because the daemon and the harness are separate processes with no shared
file access (`docs/architecture.md` "Authority and Role Bridge", Moxy repo).

Ninox already has the pieces of this problem but no live projection of them:

- **Identity env vars.** `NINOX_SESSION` and `NINOX_ORCHESTRATOR_ID` are
  exported into every spawned session
  (`crates/ninox-app/src/spawn_util.rs:285`, `crates/ninox-app/src/main.rs:1508-1510`).
  They are plain env vars — forgeable by anything in the same shell, as
  `orchestrator_auth.rs`'s own module doc says
  (`crates/ninox-core/src/orchestrator_auth.rs:4-9`).
- **A real anti-spoofing binding, but only for orchestrators.**
  `authorize_orchestrator` (`crates/ninox-core/src/orchestrator_auth.rs:14-66`)
  cross-checks a live tmux pane identity (`server_epoch`,
  `physical_tmux_name`, `pane_id`, `root_pid`, `root_created_at`) against a
  persisted `OrchestratorRuntimeIdentity` row, self-registering it on first
  use. This is ninox's existing equivalent of Moxy's signed token: proof
  that the calling process is physically running inside the pane that was
  spawned for that orchestrator ID, not just an env var carried into a
  subshell.
- **A weaker binding for workers.** `worker_status::resolve_session_id`
  (`crates/ninox-core/src/worker_status.rs:95-129`) trusts `NINOX_SESSION`
  only if it names a *live* (non-terminal) row, falling back to matching
  `cwd` against a session's `workspace_path`. The comment at
  `worker_status.rs:108-110` is explicit that the env var "can outlive its
  row's liveness" and a terminal row must not resolve. There is no
  pane/PID binding for workers today — this spec inherits that exposure
  rather than closing it (see Open Questions).
- **The store** (`crates/ninox-core/src/store.rs`) holds the session row:
  `id`, `orchestrator_id`, `status` (`SessionStatus`:
  `Spawning | Working | PrOpen | CiFailed | ReviewPending | Mergeable | Done |
  Terminated | Interrupted` — `crates/ninox-core/src/types.rs:9-20`),
  `workspace_path`, `pr_number`/`pr_id`, `agent_type`, `model`,
  `context_used_pct`, and similar — but no `role`, no task brief, and no
  explicit "revoked" state. `SessionStatus::is_terminal()`
  (`types.rs:32-34`, true for `Done | Terminated | Interrupted`) is the
  closest existing concept to Moxy's "unmanaged/revoked."
- **`ninox-server`'s HTTP routes** (`crates/ninox-server/src/routes/sessions.rs`,
  `orchestrators.rs`) expose list/mutate endpoints (`GET /api/v1/sessions/`,
  `DELETE /api/v1/sessions/:id`, `GET /api/v1/sessions/:id/diff`,
  `GET /api/v1/orchestrators/`, plus a nested terminal router —
  `sessions.rs:11-15`, `server.rs:28-33`) but no single "who am I" read,
  bind loopback-only (`crates/ninox-server/src/server.rs:19`), and carry
  **no authentication** at all — permissive CORS, no bearer token
  (`server.rs:34`). This server exists for the native app and a future web
  dashboard, not for hooks: every
  hook-facing CLI command (`ninox inbox drain-*`, `ninox worker-status
  hook-*`) instead calls `Store::open(&db_path)` directly as a local
  subprocess (`crates/ninox-app/src/main.rs:1625`, `:1663`). Because there is
  no daemon-to-harness process boundary to cross, Moxy's "domain-separated
  signed role-reader token over loopback HTTP" doesn't transfer directly —
  the trust boundary ninox actually needs to defend is "which local process
  gets to claim which session ID," which `orchestrator_auth.rs` already
  defends for orchestrators.
- **Static identity seeding is the gap.** `orchestrator_root.rs`'s
  `setup_orchestrator_root` writes `AGENTS.md`/`CLAUDE.md` and skill files
  once, and explicitly **never overwrites `AGENTS.md`/`settings.json` if they
  already exist** — comment at `orchestrator_root.rs:55`, enforced by the
  `if !agents_md_path.exists()` guard at `:78` and the matching
  `settings_path` guard at `:143`. `spawn_util.rs::seed_worker_skills` similarly seeds a
  worker's worktree with skill markdown once at spawn. None of this is
  session-specific identity; it's durable, generic capability documentation.
  A worker's actual task brief/scope is delivered only as the initial prompt
  text handed to the harness at spawn (exactly the "Goal / Scope / Workflow"
  block every worker session receives) — it is not persisted anywhere the
  store can replay. The terminal-native-runtime spec already flags "each
  worker's task brief as issued by the orchestrator" as a **new, not-yet-built**
  store field (`docs/superpowers/specs/2026-10-01-terminal-native-runtime-design.md`
  §5.2) — this spec cannot and should not invent that field.
- **Ninox already has one working per-turn, uncached reminder mechanism.**
  `inbox::drain_for_prompt_submit` (`crates/ninox-core/src/inbox.rs:191-213`)
  builds a `UserPromptSubmit` hook response carrying
  `hookSpecificOutput.additionalContext`, resolved fresh from the store on
  every invocation via `ninox inbox drain-prompt`
  (`crates/ninox-app/src/main.rs:1594-1637`), wired into a worker's
  `.claude/settings.json` by `spawn_util.rs:566-568`. This is structurally
  identical to what Moxy's role-rehydration hook does — it's just carrying
  inbox messages today, not identity.
- **Ninox's other wired hooks:** `Stop` (same inbox-drain pathway, blocking
  continued work when messages are pending — `inbox.rs:158-189`) and
  `PreToolUse` (the orchestrator subagent-blocker,
  `orchestrator_root.rs:118-150`). Nothing currently uses `SessionStart` or
  `PreCompact`.

## Goals

1. Give any session (orchestrator or worker) a way to ask "what am I" and
   get back facts resolved live from the store by its own session ID —
   never inferred from `cwd`, pane labels, or model memory.
2. Make that projection honest for sessions ninox isn't managing or has
   retired: an explicit "not managed" / "terminated" signal, never silence
   and never a guess.
3. Give Claude Code a per-turn reminder of the same facts so they survive
   context compaction without needing re-seeding, using the closest hook
   ninox's current harness actually offers.
4. Never invent task, persona, or workflow content — only identity/role
   facts the store already owns or can cheaply derive.

## Non-goals

- Implementing any of this (spec only).
- Changing or replacing the AGENTS.md/skill-seeding mechanism — this spec
  recommends it stay as the durable-capability channel and whoami become
  the live-fact channel alongside it, not instead of it.
- Closing the worker identity-binding gap relative to orchestrators (flagged
  as an open question, not solved here).
- Persisting a worker task brief in the store — that's the
  terminal-native-runtime spec's §5.2 item, a prerequisite for whoami to
  ever disclose task content, not something this spec builds.

## Proposed Design

### 1. The role descriptor

A `SessionIdentity` value, assembled fresh on every lookup from the store —
never cached, matching Moxy's "rebuilt per request" invariant:

| Field | Source |
|---|---|
| `session_id` | the row's `id` |
| `kind` | orchestrator vs. worker/standalone — `store.is_orchestrator(id)` |
| `status` | `SessionStatus`, mapped to a `managed: bool` (`!status.is_terminal()`) |
| `owner` | `orchestrator_id` (worker's parent) or, for an orchestrator, itself |
| `workspace_path`, branch | `workspace_path`; branch from the git-wrapper-captured metadata JSON (`hooks.rs:152-197`) |
| `pr` | `pr_number`/`pr_id` if set |
| `agent_type`, `model` | as stored |
| `capabilities` | the live, config-gated slice of `capabilities::REGISTRY` for this session's `Audience` — `capabilities::for_audience` (`crates/ninox-core/src/capabilities.rs:226`) filtered by `(cap.enabled)(config)`, exactly as `run_capabilities` already does (`crates/ninox-app/src/main.rs:2044-2080`) |
| `usable commands` | derived the same way: registry entries double as "what you're allowed to reach for" |

Explicitly absent: task, persona, or workflow text. Until a task-brief store
field exists (terminal-native-runtime §5.2), whoami has nothing honest to
say here and must not paraphrase the original spawn prompt from memory.

### 2. `ninox whoami`

A new hot-path CLI subcommand, dispatched alongside `Statusline` / `Inbox` /
`Capabilities` before the tmux-config/wrapper/self-shim setup in `main.rs`
(per this repo's convention for agent-invoked commands — CLAUDE.md
"Conventions"). Resolves the caller's own session the same way
`worker_status::resolve_session_id` already does (env var validated against
a live row, cwd-ancestor fallback) — read-only identity disclosure is lower
stakes than the write/authority operations `authorize_orchestrator` guards,
so it reuses the existing worker-grade resolution rather than requiring the
heavier pane-identity cross-check on every call. Supports `--json` (mirrors
`ninox capabilities --json`) and a plain-text form for direct human/agent
reading.

Unmanaged or terminal sessions get an explicit message
("ninox is not managing this session" / "this session was terminated at
`<time>`"), never a blank/empty result and never a best-effort guess from
`cwd`.

### 3. Harness-hook reminder

Claude Code's hook surface has no exact analogue to OpenCode's per-request
`context` hook (which fires on every model request, including mid-turn tool
loops). The closest available hooks, in order of relevance:

- **`UserPromptSubmit`** — fires once per human/orchestrator-submitted
  prompt. Ninox already uses it for exactly this shape of problem
  (`inbox::drain_for_prompt_submit`). Extending the same `ninox inbox
  drain-prompt` pathway (or a sibling call from the same hook entry) to also
  append a freshly-rebuilt identity reminder into `additionalContext`
  requires no new hook wiring — only a new field in the existing JSON
  response. This is the primary mechanism this spec recommends, and it is
  buildable today.
- **`SessionStart`** — fires once at session start, including after
  `--resume`. Not currently wired anywhere in ninox. Worth adding so a
  resumed session (the terminal-native-runtime spec's recovery path) gets
  an identity reminder before its first `UserPromptSubmit` fires, rather
  than starting blind.
- **`PreCompact`** — fires immediately before Claude Code compacts the
  transcript. Could carry an identity note into what the compaction step
  preserves, but it only runs at the moment of compaction, not on every
  subsequent turn — it reduces what's lost in that one instant, it does not
  by itself give the "always rebuilt, never stale" property Moxy wants.
  Treat as an optional defense-in-depth addition in a later phase, not a
  replacement for `UserPromptSubmit`.

Known gap vs. Moxy: `UserPromptSubmit` does not fire mid-turn, so a long
agentic run between two orchestrator/human prompts will not see the
reminder refresh until the next submitted prompt. A true per-step hook is a
**future-harness item** — flag it the same way the terminal-native-runtime
spec flags work that depends on infrastructure ninox doesn't have yet — and
revisit if/when Claude Code (or a future harness) exposes one.

The reminder text shares the existing `MAX_HOOK_PAYLOAD_CHARS` budget
(`inbox.rs:20`, 10,000 chars) with inbox messages in the same response, or
gets an equally small budget of its own — exact split is an implementation
decision for whichever phase builds this.

### 4. Identity binding and anti-spoofing

- For orchestrators, whoami and its hook reminder read through the same
  store row `authorize_orchestrator` already binds to a physical tmux pane.
  Nothing new is required to get "not spoofable from env alone" for the
  orchestrator case *if* a call site chooses to run the full
  `authorize_orchestrator` check — left as a per-call-site choice between
  the cheap worker-grade resolution and the stronger pane-bound one (see
  Open Questions).
- For workers, whoami inherits exactly the trust level every existing
  worker-facing hook already has (`resolve_session_id`'s env-var +
  liveness + cwd-ancestor check) — not a regression, but also not a
  strengthening. A compromised or confused process in the same shell that
  exports a different live worker's `NINOX_SESSION` can already fool
  `ninox worker-status`/`ninox inbox`; whoami would inherit that same
  exposure, now surfaced in a response that looks more "official." This
  spec does not close that gap.

### 5. Revoked / unmanaged sessions

Both the CLI command and the hook reminder must emit an explicit, honest
line whenever the resolved row is missing or `status.is_terminal()` is
true — never omit the identity block silently (a worker could read silence
as "nothing changed" rather than "you're not managed anymore") and never
synthesize a plausible-looking identity for a session ninox doesn't
recognize.

### 6. Relationship to AGENTS.md/skill seeding

AGENTS.md, CLAUDE.md, and seeded skill files stay exactly as they are: the
right home for durable, rarely-changing capability documentation ("how do I
spawn a worker," "how does the brain work"), explicitly never rewritten
after first write (`orchestrator_root.rs:55`). They are the wrong home for
anything that changes after spawn — ownership, lifecycle status, whether a
session has been revoked, or which capabilities are currently enabled by
config. whoami is a complementary, live, narrower channel for exactly that
class of fact. This spec recommends that split; it does not migrate
anything off the existing seeding mechanism.

### 7. Capability registry entry

One new `Capability` entry in `capabilities::REGISTRY`
(`crates/ninox-core/src/capabilities.rs:112`): `name: "whoami"`,
`audience: Audience::Both`, markdown for each audience explaining the
command (and, once Phase 2 ships, the reminder behavior), gated by a new
opt-in `[role_rehydration]` config section (`enabled: bool`, default off
while hook-wiring rolls out) following the `[pr_watch]`/`[auto_reap]`
pattern already established on `AppConfig`
(`crates/ninox-core/src/config.rs:523-530`). This is the single
registration point per this repo's CLAUDE.md — the markdown file plus this
one entry is the entire surface area; seeding, the AGENTS.md "Available
Skills" list, and `ninox capabilities` all derive from it automatically.

## Phasing

1. **`ninox whoami` CLI command**, read-only, store-backed, `--json` and
   text output, with the capability registry entry and skill markdown. No
   hook wiring yet — this alone replaces "grep the store by hand" for an
   agent that wants to check its own status mid-session.
2. **`UserPromptSubmit` reminder**, riding the existing
   `drain_for_prompt_submit` pathway — the only hook in this codebase with
   direct per-turn, uncached precedent.
3. **`SessionStart` reminder**, covering cold-start and `--resume`.
4. **(Future-harness item)** re-evaluate once Claude Code or a successor
   harness exposes a true per-step hook, the same way the
   terminal-native-runtime spec defers work to "the extended edition of
   Ninox." `PreCompact` wiring, if ever added, belongs here too.

## Risks

| Risk | Mitigation |
|---|---|
| Reminder grows the UserPromptSubmit payload on every turn | Share/bound the 10,000-char budget already governing inbox batching; keep the descriptor terse |
| Worker identity spoofing within the same shell | Pre-existing exposure (every worker hook has it today); explicitly documented, not silently inherited |
| Capability registry drift (markdown without a registry entry, or vice versa) | Already enforced by `capabilities.rs`'s own invariant tests (per CLAUDE.md) |
| whoami disclosing stale data during the read→apply→write window | Always read the live row at call time; never hold a cached snapshot across the hook invocation |

## Open Questions

- Should orchestrator whoami calls always run the full
  `authorize_orchestrator` pane-identity cross-check, or only when a caller
  opts into the stronger guarantee? The cheap path is consistent with every
  other worker-facing hook; the strict path is consistent with every other
  orchestrator-authority operation.
- Should worker session resolution be tightened to a pane/PID binding
  similar to orchestrators, now that whoami gives a worker's claimed
  identity more apparent authority than an inbox message did? This spec
  treats it as a pre-existing, separately-scoped gap.
- Once the terminal-native-runtime spec's task-brief store field exists,
  should whoami disclose it verbatim by default, or only on explicit
  request (to keep the default reminder small)?
- Does `PreCompact` wiring earn its complexity once `UserPromptSubmit` and
  `SessionStart` are both shipped, or does the marginal coverage not
  justify a third hook?
- Exact text budget split between inbox messages and the identity reminder
  within the shared `UserPromptSubmit` response.
