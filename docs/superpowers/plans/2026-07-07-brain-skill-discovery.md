# Brain Skill Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `brain` (and `spawn-worker`/`set-agent-config`) skills actually discoverable by Claude Code for every orchestrator, worker, and standalone session, and give workers a brain skill of their own.

**Architecture:** Move the orchestrator's generated skill files from `{root}/skills/` to `{root}/.claude/skills/` and add the YAML frontmatter Claude Code requires to register a skill. Add a new `seed_worker_brain_skill` helper in `spawn_util.rs` that writes a worker-flavored brain skill into a worker/standalone session's effective workspace and protects it from ever being committed via `.git/info/exclude`. Wire that helper into both non-orchestrator spawn call sites.

**Tech Stack:** Rust, tokio (async fs + process), existing `ninox-app` crate (`app.rs`, `main.rs`, `spawn_util.rs`).

## Global Constraints

- Skills are only discoverable by Claude Code under `.claude/skills/<name>/SKILL.md` (project-level, walks up parent directories) or `~/.claude/skills/<name>/SKILL.md` (user-level) — confirmed against Claude Code docs. A bare `skills/` directory is never scanned.
- Every `SKILL.md` needs YAML frontmatter with at least `name` and `description` fields to be registered at all.
- No enforcement hooks — guidance via skill content only (per spec's non-goals).
- Any new file-write failure must be non-fatal to a spawn (`tracing::warn!` and continue), matching `setup_orchestrator_root`'s existing failure handling in `main.rs:412-414`.
- The worker skill file must never be committable — protect it via the repo's shared `.git/info/exclude`, never the tracked `.gitignore`.

---

### Task 1: Fix orchestrator skill discovery (directory + frontmatter)

**Files:**
- Modify: `crates/ninox-app/src/app.rs:2270-2503` (`setup_orchestrator_root`)
- Test: `crates/ninox-app/src/app.rs:4519-4558` (existing tests, updated)

**Interfaces:**
- Consumes: nothing new — `setup_orchestrator_root(root: &Path, ninox_bin: &str, config_path: &str) -> anyhow::Result<()>` keeps its exact signature.
- Produces: skill files now live at `{root}/.claude/skills/{spawn-worker,set-agent-config,brain}/SKILL.md`, each with YAML frontmatter. Downstream tasks don't depend on this directly (workers get their own separate skill in Task 2/3), but this is required for the orchestrator half of the spec's goal.

- [ ] **Step 1: Update the failing tests first**

Replace the two existing tests at the end of `app.rs`'s `mod tests` (lines 4519-4558) with versions that expect the new path and frontmatter:

```rust
    #[tokio::test]
    async fn spawn_skill_teaches_work_request_handling() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("spawn-worker").join("SKILL.md"),
        ).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: spawn-worker"));
        assert!(skill.contains("description:"));
        assert!(
            skill.contains("request-work"),
            "skill must explain the worker→orchestrator work-request channel"
        );
        assert!(
            skill.contains("spawn a new worker") || skill.contains("spawn a dedicated worker"),
            "skill must tell the orchestrator to spawn a worker for requested work"
        );
        assert!(
            skill.to_lowercase().contains("never") && skill.to_lowercase().contains("widen"),
            "skill must forbid widening an existing worker's scope"
        );
    }

    #[tokio::test]
    async fn set_agent_config_skill_has_frontmatter() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("set-agent-config").join("SKILL.md"),
        ).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: set-agent-config"));
        assert!(skill.contains("description:"));
    }

    #[tokio::test]
    async fn setup_orchestrator_root_seeds_brain_skill() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let skill_path = root.join(".claude").join("skills").join("brain").join("SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: brain"));
        assert!(skill.contains("description:"));
        assert!(skill.contains("ninox brain query"));
        assert!(skill.contains("ninox brain index"));
        assert!(skill.contains("ninox brain show"));
        assert!(skill.contains("blends keyword and semantic matches"));

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(
            agents_md.contains(&skill_path.display().to_string()),
            "AGENTS.md should point orchestrators at the brain skill"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-app spawn_skill_teaches_work_request_handling set_agent_config_skill_has_frontmatter setup_orchestrator_root_seeds_brain_skill -- --nocapture`
Expected: FAIL — `spawn_skill_teaches_work_request_handling` and `setup_orchestrator_root_seeds_brain_skill` fail with a file-not-found error at the new `.claude/skills/...` path (old code still writes to `skills/...`); `set_agent_config_skill_has_frontmatter` fails the same way.

- [ ] **Step 3: Move the skill directories under `.claude/skills/`**

In `setup_orchestrator_root` (`app.rs:2270`), change:

```rust
    let claude_dir       = root.join(".claude");
    let spawn_skill_dir  = root.join("skills").join("spawn-worker");
    let config_skill_dir = root.join("skills").join("set-agent-config");
    let brain_skill_dir  = root.join("skills").join("brain");
```

to:

```rust
    let claude_dir        = root.join(".claude");
    let claude_skills_dir = claude_dir.join("skills");
    let spawn_skill_dir   = claude_skills_dir.join("spawn-worker");
    let config_skill_dir  = claude_skills_dir.join("set-agent-config");
    let brain_skill_dir   = claude_skills_dir.join("brain");
```

(The `fs::create_dir_all` calls right below already create each skill dir's full ancestry, including the new `.claude/skills` segment — no change needed there.)

- [ ] **Step 4: Add frontmatter to the spawn-worker skill content**

In the `spawn_skill_content` raw string (`app.rs:2318`), change the opening from:

```rust
    let spawn_skill_content = format!(
        r#"# Spawn a Worker, Not a Subagent
```

to:

```rust
    let spawn_skill_content = format!(
        r#"---
name: spawn-worker
description: Use before starting any implementation task as a Ninox orchestrator — spawn a worker session instead of doing the work yourself.
---

# Spawn a Worker, Not a Subagent
```

- [ ] **Step 5: Add frontmatter to the set-agent-config skill content**

In the `config_skill_content` raw string (`app.rs:2384`), change the opening from:

```rust
    let config_skill_content = format!(
        r#"# Set Ninox Agent Config
```

to:

```rust
    let config_skill_content = format!(
        r#"---
name: set-agent-config
description: Use when the user asks to change the orchestrator's or worker's agent harness or model.
---

# Set Ninox Agent Config
```

- [ ] **Step 6: Add frontmatter to the brain skill content**

In the `brain_skill_content` raw string (`app.rs:2414`), change the opening from:

```rust
    let brain_skill_content = format!(
        r#"# Read and Write the Brain
```

to:

```rust
    let brain_skill_content = format!(
        r#"---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before exploring unfamiliar code (query first) and as soon as you learn something worth keeping — write it down, don't wait until the end.
---

# Read and Write the Brain
```

- [ ] **Step 7: Sharpen the brain skill's rule wording**

In the same `brain_skill_content` string, find the closing "## The Rule" section:

```rust
## The Rule

**Query before writing, write what you find, query before exploring.** The
brain only helps the next orchestrator if you keep it current — a stale or
empty brain is no better than no brain at all.
"#,
        ninox_bin = ninox_bin,
    );
```

Replace the rule body with sharper, immediate-action wording:

```rust
## The Rule

**Before exploring anything unfamiliar, query first.** As soon as you learn
something a future session would want to know — don't wait until the end
of your session — write it down and index it. A stale or empty brain is no
better than no brain at all.
"#,
        ninox_bin = ninox_bin,
    );
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p ninox-app spawn_skill_teaches_work_request_handling set_agent_config_skill_has_frontmatter setup_orchestrator_root_seeds_brain_skill -- --nocapture`
Expected: PASS (all three)

- [ ] **Step 9: Run the full app.rs test suite to check for regressions**

Run: `cargo test -p ninox-app`
Expected: PASS — no other test references `root.join("skills")` (confirm via `grep -n '\.join("skills")' crates/ninox-app/src/app.rs` returning only the lines just changed).

- [ ] **Step 10: Commit**

```bash
git add crates/ninox-app/src/app.rs
git commit -m "fix(orchestrator): discover skills from .claude/skills with proper frontmatter"
```

---

### Task 2: Add `seed_worker_brain_skill` helper to `spawn_util.rs`

**Files:**
- Modify: `crates/ninox-app/src/spawn_util.rs` (add function + tests)

**Interfaces:**
- Produces: `pub async fn seed_worker_brain_skill(workspace: &str) -> anyhow::Result<()>` — writes `{workspace}/.claude/skills/brain/SKILL.md` and idempotently excludes it via the workspace's git-common-dir `info/exclude`. Returns `Ok(())` even when `workspace` isn't a git repo (skill file is still written; nothing to protect). Task 3 calls this by name with no other dependency.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `spawn_util.rs` (after the existing tests, before the closing `}` at line 382... i.e. inside the same `mod tests`):

```rust
    #[tokio::test]
    async fn seed_worker_brain_skill_writes_skill_with_frontmatter() {
        let dir = tempdir().unwrap();
        let ws = dir.path().to_str().unwrap().to_string();
        tokio::process::Command::new("git")
            .args(["init", "-q", &ws])
            .status()
            .await
            .unwrap();

        seed_worker_brain_skill(&ws).await.unwrap();

        let skill = tokio::fs::read_to_string(
            dir.path().join(".claude").join("skills").join("brain").join("SKILL.md"),
        )
        .await
        .unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: brain"));
        assert!(skill.contains("description:"));
        assert!(skill.contains("ninox brain query"));
    }

    #[tokio::test]
    async fn seed_worker_brain_skill_excludes_itself_from_git_idempotently() {
        let dir = tempdir().unwrap();
        let ws = dir.path().to_str().unwrap().to_string();
        tokio::process::Command::new("git")
            .args(["init", "-q", &ws])
            .status()
            .await
            .unwrap();

        seed_worker_brain_skill(&ws).await.unwrap();
        seed_worker_brain_skill(&ws).await.unwrap();

        let exclude = tokio::fs::read_to_string(dir.path().join(".git").join("info").join("exclude"))
            .await
            .unwrap();
        let count = exclude.lines().filter(|l| l.trim() == ".claude/skills/brain/").count();
        assert_eq!(count, 1, "the exclude line must appear exactly once, not duplicated");

        // The whole point of the exclude: the skill file must never show up
        // as untracked/stageable in this repo.
        let status = tokio::process::Command::new("git")
            .args(["-C", &ws, "status", "--porcelain"])
            .output()
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "the brain skill file must be excluded from git status, not merely present on disk"
        );
    }

    #[tokio::test]
    async fn seed_worker_brain_skill_skips_exclude_when_not_a_git_repo() {
        let dir = tempdir().unwrap();
        let ws = dir.path().to_str().unwrap().to_string();

        seed_worker_brain_skill(&ws).await.unwrap();

        assert!(
            dir.path().join(".claude").join("skills").join("brain").join("SKILL.md").exists(),
            "skill file must still be written even outside a git repo"
        );
        assert!(!dir.path().join(".git").exists(), "test setup sanity check: no git repo here");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-app seed_worker_brain_skill -- --nocapture`
Expected: FAIL with "cannot find function `seed_worker_brain_skill`"

- [ ] **Step 3: Implement `seed_worker_brain_skill`**

Add this function to `spawn_util.rs`, after `create_worker_worktree` (before the `#[cfg(test)]` block):

```rust
const WORKER_BRAIN_SKILL: &str = r#"---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before touching code you haven't seen before, and before finishing your task.
---

# Read and Write the Brain

The brain is Ninox's persistent, shared knowledge store. Your session's
brain is already resolved via `NINOX_BRAIN` — these commands act on it with
no extra configuration.

## Before exploring unfamiliar code

Query first — it blends keyword and semantic matches automatically:

```bash
ninox brain query "<name or concept>"
```

If a relevant entry exists, read it before you start digging through files
yourself. It may save you the exploration entirely.

## Before you finish

Write down anything you discovered that the next session — orchestrator or
worker — would otherwise have to rediscover: where something lives, why
it's built the way it is, a gotcha you hit. Create or update a Markdown
file under the section that fits:

```
repos/          where repositories live, their purpose, entry points
symbols/        where types, functions, and modules are defined
concepts/       domain terminology and mental models
patterns/       conventions and recurring implementation shapes
decisions/      why something was built a certain way (ADRs)
architecture/   how the system is structured — components, data flows
relationships/  how repos, services, and teams connect
errors/         known failure modes and how to resolve them
```

Each file needs YAML frontmatter followed by a Markdown body:

```markdown
---
type: repo
name: my-crate
tags: [auth, core]
repos: [my-crate]
updated: 2026-07-06
---

# my-crate

Entry point: `src/main.rs`
Build: `cargo build`

Facts, not prose. Link related entries with `[[other-entry]]`.
```

Then rebuild the index so the write becomes queryable:

```bash
ninox brain index
```

## The Rule

**Query before touching unfamiliar code. Write down what you found before
you're done.** A stale or empty brain is no better than no brain at all.
"#;

/// Writes a worker-flavored brain skill into `workspace` so a spawned
/// worker/standalone session sees "brain" as a real Claude Code skill from
/// the moment it starts, and makes sure the file can never end up in a
/// commit. Best-effort: any failure here should be logged and swallowed by
/// the caller, not treated as fatal to the spawn (mirrors how
/// `setup_orchestrator_root`'s own failures are handled in `main.rs`).
pub async fn seed_worker_brain_skill(workspace: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use tokio::fs;

    let skill_dir = std::path::Path::new(workspace)
        .join(".claude")
        .join("skills")
        .join("brain");
    fs::create_dir_all(&skill_dir).await.context("create .claude/skills/brain")?;
    fs::write(skill_dir.join("SKILL.md"), WORKER_BRAIN_SKILL)
        .await
        .context("write brain SKILL.md")?;

    let out = tokio::process::Command::new("git")
        .args(["-C", workspace, "rev-parse", "--git-common-dir"])
        .output()
        .await
        .context("git rev-parse --git-common-dir")?;
    if !out.status.success() {
        // Not a git repo (or git unavailable) — nothing to protect against
        // a commit; the skill file itself is still written above.
        return Ok(());
    }

    let common_dir_raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let common_dir = if std::path::Path::new(&common_dir_raw).is_absolute() {
        std::path::PathBuf::from(&common_dir_raw)
    } else {
        std::path::Path::new(workspace).join(&common_dir_raw)
    };
    let exclude_path = common_dir.join("info").join("exclude");

    const EXCLUDE_LINE: &str = ".claude/skills/brain/";
    let existing = fs::read_to_string(&exclude_path).await.unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == EXCLUDE_LINE) {
        if let Some(parent) = exclude_path.parent() {
            fs::create_dir_all(parent).await.context("create info dir")?;
        }
        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(EXCLUDE_LINE);
        updated.push('\n');
        fs::write(&exclude_path, updated).await.context("write info/exclude")?;
    }

    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-app seed_worker_brain_skill -- --nocapture`
Expected: PASS (all three)

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/spawn_util.rs
git commit -m "feat(worker): add seed_worker_brain_skill helper"
```

---

### Task 3: Wire `seed_worker_brain_skill` into worker and standalone spawn

**Files:**
- Modify: `crates/ninox-app/src/main.rs:177-183` (`run_spawn`)
- Modify: `crates/ninox-app/src/app.rs:1109-1122` (`SpawnKind::Standalone` branch)

**Interfaces:**
- Consumes: `spawn_util::seed_worker_brain_skill(workspace: &str) -> anyhow::Result<()>` from Task 2.
- Produces: nothing new for later tasks — this is the final integration point.

- [ ] **Step 1: Call it from the CLI worker path (`main.rs::run_spawn`)**

In `main.rs`, change:

```rust
    // Create an isolated git worktree so workers don't share a branch.
    // Falls back to the shared workspace if the repo check fails (e.g. not git).
    let effective_workspace = match create_worker_worktree(&workspace, &id).await {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("worktree creation failed for {id}, using shared workspace: {e}");
            workspace.clone()
        }
    };
```

to:

```rust
    // Create an isolated git worktree so workers don't share a branch.
    // Falls back to the shared workspace if the repo check fails (e.g. not git).
    let effective_workspace = match create_worker_worktree(&workspace, &id).await {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("worktree creation failed for {id}, using shared workspace: {e}");
            workspace.clone()
        }
    };
    if let Err(e) = spawn_util::seed_worker_brain_skill(&effective_workspace).await {
        tracing::warn!("failed to seed brain skill for {id}: {e}");
    }
```

Also add `seed_worker_brain_skill` to the existing `use spawn_util::{...}` import at the top of `main.rs`:

```rust
use spawn_util::{create_worker_worktree, repo_from_workspace, seed_worker_brain_skill};
```

...and use the bare name (`seed_worker_brain_skill`) instead of `spawn_util::seed_worker_brain_skill` in the call above, matching how `create_worker_worktree` is already called unqualified in this function.

- [ ] **Step 2: Call it from the standalone spawn path (`app.rs`)**

In `app.rs`, inside the `Task::future` closure of the `SpawnKind::Standalone` branch, change:

```rust
                            let effective_ws =
                                match crate::spawn_util::create_worker_worktree(&workspace, &sid).await {
                                    Ok(path) => path,
                                    Err(e) => {
                                        tracing::warn!(
                                            "worktree creation failed for {sid}, using shared workspace: {e}"
                                        );
                                        workspace.clone()
                                    }
                                };
```

to:

```rust
                            let effective_ws =
                                match crate::spawn_util::create_worker_worktree(&workspace, &sid).await {
                                    Ok(path) => path,
                                    Err(e) => {
                                        tracing::warn!(
                                            "worktree creation failed for {sid}, using shared workspace: {e}"
                                        );
                                        workspace.clone()
                                    }
                                };
                            if let Err(e) = crate::spawn_util::seed_worker_brain_skill(&effective_ws).await {
                                tracing::warn!("failed to seed brain skill for {sid}: {e}");
                            }
```

- [ ] **Step 3: Build to confirm no compile errors**

Run: `cargo build -p ninox-app`
Expected: builds cleanly (no unused-import or type errors).

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p ninox-app`
Expected: PASS — all tests from Task 1 and Task 2 still pass, no regressions elsewhere.

- [ ] **Step 5: Manual smoke check**

Run a real spawn against a scratch git repo and confirm the skill file lands where expected and is excluded:

```bash
mkdir -p /tmp/brain-skill-smoke && cd /tmp/brain-skill-smoke && git init -q
cargo run -p ninox-app -- spawn --prompt "smoke test" --workspace /tmp/brain-skill-smoke --name smoke-test
ls /tmp/brain-skill-smoke/.claude/worktrees/smoke-test/.claude/skills/brain/SKILL.md
git -C /tmp/brain-skill-smoke/.claude/worktrees/smoke-test status --porcelain  # brain skill must NOT appear
cat /tmp/brain-skill-smoke/.git/info/exclude  # must contain .claude/skills/brain/
tmux -L ninox kill-session -t smoke-test 2>/dev/null
rm -rf /tmp/brain-skill-smoke
```

Expected: the `SKILL.md` file exists; `git status --porcelain` inside the worktree shows nothing (the file is excluded, not untracked); `info/exclude` contains the new line.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src/main.rs crates/ninox-app/src/app.rs
git commit -m "feat(worker): seed the brain skill on worker and standalone spawn"
```
