# Making Orchestrators and Workers Actually Use the Brain

## Context

`docs/BRAIN.md` describes the intended loop: query the brain before
exploring unfamiliar territory, write down what you find, query again
before writing to avoid duplicates. The brain itself (`ninox-core::brain`,
`ninox brain index/query/show`) is fully implemented and working. What
isn't working is getting that loop into the head of the agents that are
supposed to run it.

Two independent, compounding bugs mean no orchestrator or worker session
has ever actually seen the brain skill as an invocable skill:

1. **Wrong directory.** `setup_orchestrator_root()`
   (`crates/ninox-app/src/app.rs:2270`) writes the `spawn-worker`,
   `set-agent-config`, and `brain` skills to `{root}/skills/<name>/SKILL.md`.
   Claude Code only auto-discovers project skills under
   `.claude/skills/<name>/SKILL.md` (walking up parent directories the same
   way it does for `CLAUDE.md`) — confirmed against Claude Code's own docs.
   A bare `skills/` directory next to `.claude/` is never scanned. The
   orchestrator's only signal today is one sentence in the generated
   `AGENTS.md` telling it to manually go read a file at an absolute path —
   no skill listing, no relevance-based invocation.
2. **Missing frontmatter.** None of the three generated `SKILL.md` bodies
   have the YAML frontmatter (`name`, `description`) Claude Code requires to
   register a skill. Even after fixing (1), these files would still not be
   recognized.
3. **Workers get nothing at all.** Worker sessions (`ninox spawn`, handled
   by `run_spawn` in `crates/ninox-app/src/main.rs:137`) and standalone
   sessions (`SpawnKind::Standalone` in `crates/ninox-app/src/app.rs:1017`)
   both already receive a working `NINOX_BRAIN` environment variable
   (pointing at the resolved brain catalogue), but no skill file, no
   `AGENTS.md`, nothing — they only get a plain-text prompt footer
   (`worker_context_footer()`, `main.rs:273`) covering `ninox send`/
   `ninox request-work`. The CLI works if invoked, but nothing tells a
   worker to invoke it.

Orchestrators and standalone/worker sessions are the two shapes of
non-orchestrator, single-task agent in Ninox — both already share the same
worktree-isolation plumbing (`spawn_util::create_worker_worktree`) and the
same fallback-to-shared-workspace behavior when that repo isn't a git repo.
"Workers," for the purposes of this spec, means both.

## Goal

Every orchestrator and every worker/standalone session has the brain skill
show up as a real, Claude-Code-discovered skill from the moment it starts,
so the existing query-before-exploring / write-what-you-find loop in
`docs/BRAIN.md` actually happens without a human reminding the agent.

## Non-goals

- **No enforcement hooks.** No Stop-hook or PreToolUse nudge forcing a
  brain write before a session ends. This is a deliberate choice: it keeps
  the fix consistent with how `spawn-worker` already works today (skill
  content the agent chooses to follow, not a hard mechanical gate). The
  only existing hook (`subagent-blocker.cjs`) enforces a hard tool block,
  which is a different kind of guarantee than "did you write down what you
  learned" — that's not mechanically checkable anyway.
- **No changes to the brain's own CLI/index/query semantics.** `BrainIndex`,
  `ninox brain index/query/show`, and the resolution order in `docs/BRAIN.md`
  are unchanged and already correct.
- **No edits to a worker's real project files.** Worker brain access is
  entirely via a new `.claude/skills/brain/SKILL.md` written into the
  worker's own isolated worktree (or shared workspace on worktree-creation
  fallback) — never into the user's own `AGENTS.md`/`CLAUDE.md`, and never
  committed into the worker's branch (see Architecture).
- **No content changes to `spawn-worker`/`set-agent-config` beyond
  frontmatter.** They share the same directory + frontmatter bug as `brain`
  and get the same mechanical fix, but their instructional content is out
  of scope here.

## Approach

1. Move the orchestrator's generated skill directories from
   `{root}/skills/` to `{root}/.claude/skills/`, and give all three
   generated `SKILL.md` files proper YAML frontmatter.
2. Add a new shared helper that writes a worker-flavored
   `.claude/skills/brain/SKILL.md` into a worker's effective workspace at
   spawn time, and protects that file from ever being committed by
   excluding it at the git level (not the tracked `.gitignore`).
3. Sharpen the imperative language in the brain skill content for both
   roles (query before touching unfamiliar code; write down what you found
   before finishing) — wording only, no new mechanism.

## Architecture

### Orchestrator: directory + frontmatter fix (`app.rs`)

In `setup_orchestrator_root()`:

```rust
let claude_skills_dir = claude_dir.join("skills");           // was: root.join("skills")
let spawn_skill_dir    = claude_skills_dir.join("spawn-worker");
let config_skill_dir   = claude_skills_dir.join("set-agent-config");
let brain_skill_dir    = claude_skills_dir.join("brain");
```

(`claude_dir` already exists as `root.join(".claude")`, used today for
`settings.json`/`subagent-blocker.cjs`.)

Each generated `SKILL.md` body gains frontmatter, e.g.:

```markdown
---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before exploring unfamiliar code (query first) and before finishing a task (write down what you found).
---

# Read and Write the Brain
...
```

`name`/`description` are picked to match the existing plain-file pattern:
`spawn-worker` ("Use when starting any implementation task — spawn a
worker instead of doing it yourself"), `set-agent-config` ("Use when the
user asks to change the agent harness or model"), `brain` (as above).

The generated `AGENTS.md` keeps its "Available Skills" section as a
friendly pointer/reinforcement, but it stops being the *only* signal —
Claude Code will now list all three skills natively regardless of whether
the agent ever reads `AGENTS.md`.

The two existing tests referencing `root.join("skills")`
(`spawn_skill_teaches_work_request_handling`,
`setup_orchestrator_root_seeds_brain_skill`) move to
`root.join(".claude").join("skills")` and additionally assert the
frontmatter (`name:`/`description:`) is present.

### Worker/standalone: new skill file + git-exclude safeguard

New function in `spawn_util.rs` (the existing home for plumbing shared
between the CLI worker path and the app's standalone spawn path):

```rust
/// Writes a worker-flavored brain skill into `workspace` and makes sure it
/// can never be committed. Best-effort: failures are logged, never fatal
/// to the spawn (mirrors `setup_orchestrator_root`'s own failure handling).
pub async fn seed_worker_brain_skill(workspace: &str)
```

Behavior:

1. Write `{workspace}/.claude/skills/brain/SKILL.md` — frontmatter +
   worker-flavored body (see Content below).
2. Resolve the git-common-dir for `workspace` via
   `git -C {workspace} rev-parse --git-common-dir` (works whether
   `workspace` is a plain repo checkout or a `git worktree add` worktree,
   unlike hardcoding `.git/info/exclude`). If this fails (not a git repo,
   e.g. the non-git fallback branch some callers already handle elsewhere),
   skip step 3 entirely — no exclude file to write, nothing to protect,
   matches `repo_from_workspace`'s existing "return None/skip on failure"
   shape.
3. Idempotently append `.claude/skills/brain/` to
   `{git-common-dir}/info/exclude` — read the file (empty string if
   missing), skip if the line is already present, otherwise append it.
   `info/exclude` is per-repository (shared across all of that repo's
   worktrees, since it lives in the common `.git` dir, not a per-worktree
   file) and is itself untracked, so this can never itself show up in a
   diff or be part of a commit. Because git ignore rules never affect
   already-tracked files, this is safe even for a project that
   legitimately commits its own `.claude/skills/` directory for unrelated
   skills — only the new, still-untracked `brain/` subdirectory is
   affected.

Call sites — both call this right after their existing
`create_worker_worktree` attempt, success or fallback, using whatever path
ends up as the effective workspace (mirrors how `NINOX_BRAIN` already gets
attached the same way regardless of which branch was taken):

- `main.rs::run_spawn`, right after `effective_workspace` is resolved
  (`main.rs:177-183`).
- `app.rs`'s `SpawnKind::Standalone` branch, right after `effective_ws` is
  resolved (`app.rs:1109-1122`).

`SpawnKind::Orchestrator` sessions need no change here — their workspace is
always a direct subdirectory of `orchestrator_root`, which already has
`.claude/skills/` at its root (per the orchestrator fix above); Claude
Code's parent-directory walk picks it up the same way it already does for
`CLAUDE.md`.

### Content: sharper imperative wording

Both skill bodies (orchestrator's existing `brain` skill, and the new
worker-flavored one) keep the existing three-step rule from `docs/BRAIN.md`
but tighten the trigger language so it reads as "do this now," not
"here's a feature that exists":

- **Orchestrator** (`app.rs`'s `brain_skill_content`): unchanged structure,
  reworded rule: *"Before exploring anything unfamiliar, query first. As
  soon as you learn something a future session would want — don't wait
  until the end — write it down and index it."*
- **Worker** (new, in `spawn_util.rs`): trimmed for a single-task session —
  no "browse architecture/repo entries at the start of a session" framing
  (that's an orchestrator concern), just: *"Before touching code you
  haven't seen before, run `{ninox_bin} brain query \"<name or concept>\"`.
  Before you open the PR, write down anything you discovered that the next
  session — orchestrator or worker — would otherwise have to rediscover
  (where something lives, why it's built that way, a gotcha you hit)."*
  Same frontmatter shape, same Markdown-with-frontmatter write format
  reference as the orchestrator version.

## Error handling

- `seed_worker_brain_skill`'s file write failing (permissions, disk) →
  `tracing::warn!`, spawn proceeds without the skill — same
  degrade-safely posture as `setup_orchestrator_root`'s own callers today
  (`main.rs:412-414` already just warns and continues on that function's
  failure).
- `git rev-parse --git-common-dir` failing (workspace isn't a git repo at
  all) → skip the exclude step silently; there's no commit risk to guard
  against in a non-git workspace, and no brain-skill value lost — the file
  is still written.
- Concurrent workers spawning in the same repo simultaneously and both
  appending to `info/exclude` → idempotent check-then-append makes a
  duplicate line harmless even in the unlikely event of a race; worst case
  is a harmless duplicate entry, never a lost one (append-only, no
  rewrite-in-place).

## Testing

- `app.rs`: update `spawn_skill_teaches_work_request_handling` and
  `setup_orchestrator_root_seeds_brain_skill` to the new
  `.claude/skills/` paths; add frontmatter assertions (`name:`,
  `description:`) to both, plus a new assertion that
  `set-agent-config`'s `SKILL.md` also has frontmatter.
- `spawn_util.rs`:
  - `seed_worker_brain_skill` writes `.claude/skills/brain/SKILL.md` with
    frontmatter into a temp git repo.
  - Second call is idempotent — `info/exclude` gains exactly one
    `.claude/skills/brain/` line, not two.
  - Called against a non-git temp directory — returns `Ok(())` (or
    logs-and-continues, matching the function's `Result` shape), skill
    file still written, no panic.
- `main.rs::run_spawn` / `app.rs` Standalone: a spawn (using the existing
  tmux-backed test pattern already in `spawn_util.rs`'s test module)
  results in `.claude/skills/brain/SKILL.md` existing in the effective
  workspace afterward.
