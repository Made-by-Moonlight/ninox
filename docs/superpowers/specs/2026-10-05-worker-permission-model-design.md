# Worker Permission Model: Merge Gates and Standing Permissions

**Status:** Proposal — design spec only (MLOPS-4668). No implementation in
this change.

## Context

Every `claude-code` worker launch and every `claude-code` resume hardcodes
`--dangerously-skip-permissions` (`crates/ninox-core/src/harness.rs`
`builtin_specs()`, `worker_args` at line 69 and `resume_args` at line 75).
Orchestrator sessions use `interactive_args` instead, which carries no skip
flag — they get Claude Code's normal permission prompting plus whatever
`settings.json` `permissions` rules a human has configured.

The skip flag exists because a worker is headless: nothing is attached to
answer Claude Code's "allow this tool call? y/n" prompt, and an unanswered
prompt just hangs the session forever. The flag is also all-or-nothing —
every tool call is pre-approved, with no distinction between `cargo test`
and `gh pr merge`. Today the only thing standing between a worker and
merging its own PR, force-pushing, or deleting the branch out from under
the orchestrator is a sentence in that worker's spawn prompt, which is
convention, not enforcement.

### What's already in the codebase to build on

- **A PreToolUse deny-with-reason hook already ships**, just not for
  workers. `orchestrator_root.rs::setup_orchestrator_root` writes a
  `subagent-blocker.cjs` into `<orchestrator root>/.claude/` that reads the
  hook's JSON payload off stdin and, for `Task`/`Agent` tool calls, emits:

  ```json
  {"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "..."}}
  ```

  This is the precedent to extend for a merge gate: same hook shape, same
  "deny one named tool call with a reason" mechanic, different trigger
  condition and a different seeding location (worker worktree instead of
  orchestrator root).

- **The hook only fires for orchestrators today**, gated by
  `NINOX_CALLER_TYPE`. `main.rs`'s `caller_is_orchestrator` comments spell
  out why that env var is *not* a security boundary against a hostile
  agent: it "lives in the agent's own shell, so a worker could simply
  export `NINOX_CALLER_TYPE=orchestrator`" — which is why destructive CLI
  commands like `ninox reap` resolve caller identity from the **store** via
  `NINOX_SESSION`, falling back to the env var only when there's no session
  to look up. The PreToolUse hook itself doesn't have this problem in the
  same way — it's invoked by the `claude` binary, not by the agent's own
  shell, so the agent can't forge the hook's inputs the way it can forge an
  env var. But it *can* edit or delete the hook file and `settings.json`
  that reference it, since both live inside the worktree it has full write
  access to. See the Risks section below — this matters for how much
  enforcement weight a worktree-local hook can actually bear.

- **A second, independent interception layer already exists below Claude
  Code's own hooks**: `hooks.rs` installs `gh` and `git` wrapper scripts
  into a shared `ninox_bin_dir` that's PATH-prepended for *every* spawned
  session (worker and orchestrator alike — see `spawn_util.rs`'s
  `launch_interactive_session` and the equivalent worker path in
  `main.rs`). The `gh` wrapper already intercepts `gh pr create` to capture
  PR metadata; it's shell-level, not harness-level, and today shared across
  all sessions rather than scoped per-worktree. Worth naming as an
  alternative place a merge gate *could* live, with different tradeoffs
  (see Open Questions).

- **`gate_status` already means something else.** `ninox_core::types`
  has a `GateStatus { ci, review, mergeable }` — informational polling of a
  PR's merge-readiness signals from GitHub, surfaced in the UI. This spec's
  "merge gate" is an enforcement concept (stop the tool call), unrelated to
  that polling concept. Naming in any implementation should avoid colliding
  the two (e.g. call the config/hook something other than `gate`).

- **Settings seeding into worker worktrees already happens once per
  spawn.** `spawn_util.rs::ensure_statusline_settings` writes
  `.claude/settings.json` (statusLine + `Stop`/`UserPromptSubmit` hooks for
  worker-status and opt-in inbox messaging) into every freshly created
  worktree, write-if-absent, re-run on every respawn path
  (`create_worktree_at`, `ensure_session_workspace`). Any standing-permission
  or merge-gate config needs to compose into this same write, the same way
  inbox-drain hooks and worker-status hooks already compose as siblings on
  the same event arrays.

- **The opt-in-config pattern is established**: `[inbox_messaging]` /
  `[pr_watch]` are `#[serde(default)]` structs (`PrWatchConfig { enabled:
  bool }`) declared on `AppConfig` before the `harnesses` field (TOML
  table-ordering constraint — scalars must serialize first). A
  `[worker_permissions]` block should follow the same shape.

### What Claude Code itself offers as a middle ground

Claude Code's `settings.json` supports a `permissions` block with
`allow`/`deny`/`ask` arrays of tool-call patterns (e.g. `"Bash(cargo
test:*)"`, `"Edit"`, `"Bash(gh pr merge:*)"`), evaluated in that precedence
order, plus the `PreToolUse`/`PostToolUse` hook mechanism already used by
the subagent-blocker. This is the real alternative to the blanket skip
flag: drop `--dangerously-skip-permissions`, and let Claude Code's own
permission engine enforce a scoped allow-list instead of either "everything
pre-approved" or "everything prompts into the void."

The catch: anything not covered by an `allow` or `deny` rule falls through
to `ask` by default — and a worker has no one to answer `ask`. So a
standing allow-list has to be broad enough that routine worker activity
(file edits, `cargo build`/`test`/`clippy`, `git` read/write, `gh pr
create`/`view`/`diff`) never hits that gap, while the genuinely dangerous
actions are explicit `deny` entries (hard, immediate, no hang) rather than
`ask` entries (which would hang just like today's prompt problem, only
worse because nothing today handles an unanswered `ask` for a headless
session). This reframes "standing permissions" as: get `allow` wide enough
to cover everything expected, get `deny` to cover everything that must
never run unattended, and treat anything landing outside both as a design
gap to close, not a runtime prompt to tolerate.

## Non-goals

- Implementing any of this. Spec only.
- Changing orchestrator permission behavior. Orchestrators keep
  `interactive_args` and normal Claude Code prompting as-is.

## Proposed design

### 1. Standing permissions — scoped allow-list

A new opt-in config block, following the `[pr_watch]` shape but with rule
lists instead of a single toggle:

```toml
[worker_permissions]
enabled = false
allow = [
  "Edit", "Write", "Read", "Grep", "Glob",
  "Bash(cargo build:*)", "Bash(cargo test:*)", "Bash(cargo clippy:*)",
  "Bash(git add:*)", "Bash(git commit:*)", "Bash(git checkout:*)",
  "Bash(git diff:*)", "Bash(git log:*)", "Bash(git status:*)",
  "Bash(gh pr create:*)", "Bash(gh pr view:*)", "Bash(gh pr diff:*)",
  "Bash(gh issue:*)",
]
deny = [
  "Bash(gh pr merge:*)", "Bash(git push --force:*)", "Bash(git push -f:*)",
]
ask = []
```

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerPermissionsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
}
```

Declared on `AppConfig` immediately before `harnesses` (same TOML-ordering
reason the doc comment there already gives).

When `enabled`, two things change:

- `harness.rs`'s `claude-code` `worker_args`/`resume_args` stop including
  `--dangerously-skip-permissions`.
- Worker-worktree seeding (alongside `ensure_statusline_settings`) writes a
  `permissions` block into `.claude/settings.json` built from the config's
  `allow`/`deny`/`ask` lists, using Claude Code's own rule syntax verbatim
  rather than inventing a ninox-specific DSL — see Open Questions on this
  choice.

Rule syntax is Claude Code's own `permissions.allow`/`deny`/`ask` pattern
language, so config authors can lean on upstream Claude Code docs rather
than a ninox-specific format, at the cost of ninox not being able to
validate or abstract over it.

### 2. Merge gate — a worker-side PreToolUse hook

A second hook file, seeded the same way the subagent-blocker is (Node
script, reads the PreToolUse JSON payload off stdin, writes a deny
decision), but:

- seeded per worker worktree (via the same seeding path as
  `ensure_statusline_settings`, since every worktree is fresh per spawn),
  not once into a shared orchestrator root;
- matches `Bash` tool calls whose `tool_input.command` looks like `gh pr
  merge`, `git push --force`/`-f`, or similar irreversible actions, instead
  of matching on `tool_name`/`subagent_type` like the orchestrator's
  blocker does;
- always returns `deny`, never `ask` — for a headless worker, `ask` isn't
  a softer version of `deny`, it's a silent hang. The denial reason text
  should point at the escape hatch (`ninox request-work "<description>"`),
  mirroring the subagent-blocker's "use X instead of Y" phrasing.

This is a convention-reinforcing speed bump, not a hard boundary: the
worker has full write access to its own `.claude/settings.json` and hook
file and can edit or delete either before calling `gh pr merge`. The
`NINOX_CALLER_TYPE` lesson from `main.rs` generalizes here — anything whose
enforcement lives entirely inside the thing it's meant to constrain is
soft. **The hard boundary is GitHub branch protection** (required reviews,
required status checks, disallowing self-approval) at the repo level,
which holds regardless of what happens inside the worktree. Recommend
shipping both: the hook for fast in-session feedback and to make the
convention self-enforcing in the common case, branch protection as the
backstop that holds even if the hook is bypassed, removed, or never seeded
(e.g. an older worktree, a non-claude-code harness with no hook support).

### Seeding gap: write-if-absent settings.json

`ensure_statusline_settings` (`spawn_util.rs`) returns immediately if
`.claude/settings.json` already exists — deliberately, so a copy checked
into the branch (the scenario its own test,
`create_worker_worktree_preserves_existing_settings_json`, exercises) isn't
clobbered. Composing the `permissions` block and the merge-gate hook into
that same write means a worktree whose branch already carries a
`settings.json` gets **neither** feature seeded at all — silently, with no
error and no log distinguishing it from a worktree where they were seeded
normally. That's a worse failure mode than Open Question 5's tamper risk:
tampering is at least something a detection sweep could notice, where this
is a well-behaved existing feature (preserve the user's committed
settings) quietly defeating a security control. Any implementation needs
an explicit answer here — e.g. merge the `permissions`/hook keys into an
existing `settings.json` instead of skipping the whole file, rather than
inheriting the write-if-absent behavior wholesale.

### Addendum: a third trust boundary — worker-to-own-subagent containment

The merge gate and standing permissions above both operate at the
`PreToolUse` layer, which fires for every tool call Claude Code dispatches
inside a session — the hook has no way to tell whether the main loop or a
forked subagent (the `Task`/`Agent` tool) issued a given call. That's
incidentally useful: the merge-gate hook in §2 already applies to a
worker's own forked subagents without extra work, since it can't
distinguish the caller either way.

But that symmetry only covers what the hook actually matches
(`gh pr merge`, force-push). Nothing today gates the much larger set of
other consequential, cross-session-visible actions a worker's own forked
subagent could take on its own initiative — `git commit`/`git push`,
`gh pr create`, or `ninox send` to another session — independently of
whatever narrower task the worker's main loop instructed it for. This is a
third, distinct trust boundary from the two this spec otherwise covers
(orchestrator↔worker identity, worker↔GitHub merge actions):
worker-main-loop↔worker's-own-forked-subagent.

The orchestrator side of this already has a hook: `setup_orchestrator_root`
denies the `Task`/`Agent` tool outright for orchestrators (steering them to
`ninox spawn` instead). Workers get no equivalent — `seed_worker_skills`
seeds skill markdown plus the statusline/worker-status/inbox hooks, nothing
that gates the `Agent`/`Task` tool or named commands for workers. (Verified
directly: `PreToolUse` appears nowhere in this codebase outside
`orchestrator_root.rs`.)

Unlike orchestrators, "deny all subagents" isn't the right default for a
worker — forked subagents are a legitimate tool for a worker's own research
and parallel exploration, unlike for an orchestrator, which is banned from
them entirely. So the fix isn't a blanket blocker; it's some combination
of (a) widening the same tool-call-level gate mechanism as the merge gate
to cover commit/push/PR-create/cross-session-send regardless of which
caller inside the session issued them, and (b) skill-doc guidance (the
same mechanism `seed_worker_skills` already uses for other worker-facing
instructions) that consequential, cross-session-visible actions belong to
the worker's own main loop and should never be delegated to a forked
subagent — a convention the mechanism in (a) can't yet enforce on its own.

Named here as a gap this spec doesn't solve: closing it fully means
widening the gate list well past merge/force-push, which is a larger scope
than the ticket asked for. See Open Question 9.

## Addressing the ticket's explicit questions

**Per-repo / per-orchestrator differentiation.** `[worker_permissions]` as
sketched above is global on `AppConfig`, matching `[pr_watch]` and
`[inbox_messaging]` today (also global, not per-repo). If per-repo
allow-lists turn out to be needed, the `[harnesses.*]` `BTreeMap` override
pattern is the available precedent to extend to — but that's more
structure than today's any other permission-adjacent config carries, so
starting global and widening later (rather than guessing the right
per-repo shape now) is the lower-risk default. Flagged as an open question
below rather than decided.

**A worker needing something outside its standing grant.** Given workers
have no interactive channel and an unanswered `ask` is a hang, not a
graceful wait, the only shape that fits the current architecture is: the
hook denies, the denial reason tells the worker to call `ninox
request-work "<description>"`, and the worker either proceeds with other
parts of its task or reports itself blocked. There is no "blocking wait for
human confirmation" primitive today, and building one is a meaningfully
bigger feature (a synchronous request/response channel to an orchestrator
that's also potentially unattended) than this spec's scope. Recommend hard
deny + `request-work` as the default answer, called out explicitly as a
policy choice in Open Questions since "build a blocking-approval channel
instead" is a real, larger alternative.

**Should orchestrators eventually get this too?** Orchestrators use
`interactive_args` and rely on a human being attached to answer prompts —
but that assumption already weakens once unattended orchestrator
restart/autostart ships (see
`docs/superpowers/specs/2026-10-01-terminal-native-runtime-design.md`'s
`restore_policy = auto`, future work, not scheduled). An autostarted
orchestrator is exactly as headless as a worker. Out of scope to decide
here, but the parallel is real and worth the open question below rather
than silently assuming orchestrators stay exempt forever.

## Open questions

These are genuine policy calls, not implementation details — listed
without a silently-assumed answer where there's a real tradeoff:

1. **Opt-in vs. opt-out rollout.** Should `[worker_permissions].enabled`
   default to `false` (like `pr_watch`/`inbox_messaging`, safe but leaves
   today's fully-open behavior as the default) or should this ship as the
   new default once proven, given it directly closes a real gap? Existing
   worker spawn prompts are written assuming full autonomy; flipping the
   default changes behavior for every existing worker prompt, not just new
   ones.
2. **Non-overridable deny floor.** Should certain actions (self-merge,
   force-push to the default branch) be hardcoded into the seeded hook
   regardless of what a user puts in `[worker_permissions].deny` — i.e. a
   floor the config can raise but never lower — or should operators be
   able to override even that for repos where self-merge is intentionally
   fine (e.g. a low-stakes internal tool repo)?
3. **Rule syntax ownership.** Use Claude Code's own `permissions`
   allow/deny/ask pattern strings verbatim in ninox config (simple, but
   ninox can't validate or abstract over it, and it couples ninox config
   to Claude Code's specific syntax — awkward for other harnesses), or
   define a ninox-level DSL that compiles down to each harness's native
   permission format (more work, more indirection, but harness-portable)?
4. **Command-string matching is gameable.** A `Bash`-command-regex hook
   catches literal `gh pr merge`/`git push --force` but not an alias, a
   wrapper script, a different invocation shape (`git push -f`), or a
   direct GitHub API call via `curl`. Is that an acceptable gap given
   branch protection is the real backstop, or does the hook need to be
   paired with something that can't be text-pattern-evaded (e.g. scoping
   the worker's `gh`/git credentials themselves rather than trying to
   catch every spelling of the forbidden command)?
5. **Worktree-local enforcement is soft by construction.** The worker can
   edit or delete its own `.claude/settings.json`/hook file. Is that an
   accepted limitation (document it, lean on branch protection), or should
   ninox do something about tamper-evidence (e.g. a periodic check that
   flags a session whose seeded hook file no longer matches what was
   seeded)?
6. **Escalation shape.** Is hard-deny-and-`request-work` (described above)
   the right default, or is a blocking-approval channel (worker waits,
   orchestrator or a human explicitly unblocks it) worth building as a
   separate, larger feature? The two aren't mutually exclusive, but they're
   very different amounts of work.
7. **Parallel orchestrator treatment.** Out of scope for this change by the
   ticket's own framing, but: should a follow-up track orchestrators once
   unattended autostart (terminal-native-runtime spec) ships, given the
   same headless-with-nobody-to-prompt problem reappears there?
8. **Pre-existing settings.json seeding gap.** Given the write-if-absent
   behavior described above, should seeding switch to merging the
   `permissions`/hook keys into an existing `settings.json` (more seeding
   logic, but no silent gap), or is a one-time startup check that warns
   when a worktree's committed `settings.json` predates and lacks these
   keys an acceptable, cheaper alternative?
9. **Worker-to-own-subagent containment.** Should the tool-call gate list
   widen beyond merge/force-push to cover every consequential,
   cross-session-visible action (commit, push, `gh pr create`, `ninox
   send`) so a worker's own forked subagent can't take them unsupervised —
   and if so, is skill-doc guidance (the worker's main loop owns these
   actions, subagents never do) sufficient on its own, or does it need the
   same hook-layer backing the merge gate gets? This is a bigger scope than
   the ticket asked for; named here as a gap to weigh, not solved by this
   spec.
