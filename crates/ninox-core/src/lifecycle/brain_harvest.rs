//! Background brain harvest: when a worker session's PR is first detected
//! (see `poller::sync_sessions_metadata`), a short-lived `claude -p`
//! subprocess reads the session's diff and writes structured facts into the
//! brain vault, then reindexes. Triggered automatically — never part of a
//! worker's own interactive behavior.

use anyhow::Result;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};
use tokio::process::Command;

/// Filenames whose changes alone are never worth a harvest — lockfiles churn
/// on every dependency bump with no facts or decisions to record.
const TRIVIAL_FILENAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Gemfile.lock",
];

/// Diffs larger than this are truncated before being inlined into the
/// harvest prompt, to stay well under any shell/argv length limit.
const MAX_INLINE_DIFF_BYTES: usize = 60_000;

/// Runs the harvest subprocess. Production code uses [`ClaudeHarvestRunner`];
/// tests inject a fake so they never spawn a real process or make a network
/// call.
pub trait HarvestRunner: Send + Sync {
    fn run(
        &self,
        prompt: String,
        workspace: PathBuf,
        brain_path: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;
}

/// Shells out to a real, one-shot, non-interactive `claude -p` invocation.
/// `--dangerously-skip-permissions` is required here, not optional: a
/// headless `-p` process has no TTY to approve the file writes and `ninox
/// brain index` call the harvest prompt asks for.
pub struct ClaudeHarvestRunner;

impl HarvestRunner for ClaudeHarvestRunner {
    fn run(
        &self,
        prompt: String,
        workspace: PathBuf,
        brain_path: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            let output = Command::new("claude")
                .args(["--dangerously-skip-permissions", "-p", &prompt])
                .current_dir(&workspace)
                .env("NINOX_BRAIN", &brain_path)
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!(
                    "claude -p exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim(),
                );
            }
            Ok(())
        })
    }
}

/// The repo's default branch, preferring the remote-tracking symref every
/// `git clone` sets (`origin/<name>`) and falling back to a local `main` or
/// `master` branch when that symref is missing (e.g. no `origin` remote).
async fn detect_default_branch(workspace: &Path) -> String {
    let ws = workspace.to_string_lossy();

    if let Ok(out) = Command::new("git")
        .args(["-C", &ws, "symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
        .await
    {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }

    for candidate in ["main", "master"] {
        if let Ok(out) = Command::new("git")
            .args(["-C", &ws, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{candidate}")])
            .output()
            .await
        {
            if out.status.success() {
                return candidate.to_string();
            }
        }
    }

    "main".to_string()
}

/// The session's diff against the repo's default branch, or `None` when
/// there's nothing worth harvesting: no diff at all, or a diff touching only
/// lockfiles.
pub async fn compute_nontrivial_diff(workspace: &Path) -> Option<String> {
    let default_branch = detect_default_branch(workspace).await;
    let range = format!("{default_branch}...HEAD");
    let ws = workspace.to_string_lossy();

    let names_out = Command::new("git")
        .args(["-C", &ws, "diff", "--name-only", &range])
        .output()
        .await
        .ok()?;
    if !names_out.status.success() {
        return None;
    }
    let names = String::from_utf8_lossy(&names_out.stdout);
    let files: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
    if files.is_empty() {
        return None;
    }
    let all_trivial = files.iter().all(|f| {
        let base = Path::new(f).file_name().and_then(|n| n.to_str()).unwrap_or(f);
        TRIVIAL_FILENAMES.contains(&base)
    });
    if all_trivial {
        return None;
    }

    let diff_out = Command::new("git")
        .args(["-C", &ws, "diff", &range])
        .output()
        .await
        .ok()?;
    if !diff_out.status.success() {
        return None;
    }
    let diff = String::from_utf8_lossy(&diff_out.stdout).to_string();
    if diff.trim().is_empty() {
        return None;
    }
    Some(diff)
}

/// Build the one-shot harvest prompt. Inlines the same brain workflow
/// `WORKER_BRAIN_SKILL` teaches an interactive worker — query before
/// writing, categorized Markdown with YAML frontmatter, reindex when done —
/// since a headless `-p` invocation has no skill-loading step of its own.
pub fn build_harvest_prompt(session_id: &str, diff: &str) -> String {
    let diff_body = if diff.len() > MAX_INLINE_DIFF_BYTES {
        let shown = String::from_utf8_lossy(&diff.as_bytes()[..MAX_INLINE_DIFF_BYTES]);
        format!(
            "{shown}\n\n[diff truncated — {} more bytes not shown; the full diff is available via `git diff` in this worktree]",
            diff.len() - MAX_INLINE_DIFF_BYTES,
        )
    } else {
        diff.to_string()
    };

    format!(
        r#"You are Ninox's brain-harvest agent, a one-shot background task that just ran after worker session `{session_id}` opened a pull request. Read the diff below and write down anything a future session would otherwise have to rediscover — where something lives, why it's built the way it is, a gotcha, a decision. Skip writing anything if the diff genuinely has nothing worth recording.

The brain is Ninox's persistent, shared knowledge store, already resolved via the `NINOX_BRAIN` environment variable.

Before writing:
1. Run `ninox brain query "<topic>"` for anything the diff touches, to avoid writing a duplicate entry.
2. Write or update Markdown files under the section that fits:
   repos/          where repositories live, their purpose, entry points
   symbols/        where types, functions, and modules are defined
   concepts/       domain terminology and mental models
   patterns/       conventions and recurring implementation shapes
   decisions/      why something was built a certain way (ADRs)
   architecture/   how the system is structured — components, data flows
   relationships/  how repos, services, and teams connect
   errors/         known failure modes and how to resolve them
3. Each file needs YAML frontmatter (type, name, tags, repos, updated) followed by a Markdown body of facts, not prose. Link related entries with `[[other-entry]]`.
4. Run `ninox brain index` to rebuild the index once you're done writing.

Diff to harvest from:

```diff
{diff_body}
```
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A repo with an explicit `main` branch and one commit, so
    /// `detect_default_branch`'s local-branch fallback (no `origin` remote
    /// in these fixtures) resolves deterministically regardless of the
    /// machine's `init.defaultBranch` setting.
    fn init_repo() -> std::path::PathBuf {
        let dir = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    fn checkout_feature_branch(repo: &std::path::Path, name: &str) {
        std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "checkout", "-q", "-b", name])
            .output()
            .unwrap();
    }

    fn write_and_commit(repo: &std::path::Path, file: &str, contents: &str, message: &str) {
        std::fs::write(repo.join(file), contents).unwrap();
        std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "add", file])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "commit", "-q", "-m", message])
            .output()
            .unwrap();
    }

    #[tokio::test]
    async fn no_diff_from_default_branch_returns_none() {
        let repo = init_repo();
        checkout_feature_branch(&repo, "feature-1");
        assert!(compute_nontrivial_diff(&repo).await.is_none());
    }

    #[tokio::test]
    async fn nontrivial_diff_is_returned() {
        let repo = init_repo();
        checkout_feature_branch(&repo, "feature-2");
        write_and_commit(&repo, "src.rs", "fn main() {}\n", "add source file");

        let diff = compute_nontrivial_diff(&repo).await.expect("non-trivial diff");
        assert!(diff.contains("src.rs"));
        assert!(diff.contains("fn main()"));
    }

    #[tokio::test]
    async fn lockfile_only_diff_is_skipped() {
        let repo = init_repo();
        checkout_feature_branch(&repo, "feature-3");
        write_and_commit(&repo, "Cargo.lock", "version = 3\n", "bump lockfile");

        assert!(
            compute_nontrivial_diff(&repo).await.is_none(),
            "a diff touching only a lockfile must not trigger a harvest",
        );
    }

    #[tokio::test]
    async fn mixed_lockfile_and_source_diff_is_not_skipped() {
        let repo = init_repo();
        checkout_feature_branch(&repo, "feature-4");
        write_and_commit(&repo, "Cargo.lock", "version = 3\n", "bump lockfile");
        write_and_commit(&repo, "src.rs", "fn main() {}\n", "add source file");

        let diff = compute_nontrivial_diff(&repo).await.expect("non-trivial diff");
        assert!(diff.contains("src.rs"));
    }

    #[test]
    fn harvest_prompt_includes_session_and_diff_and_workflow() {
        let prompt = build_harvest_prompt("sess-1", "diff --git a/x b/x\n+hello\n");
        assert!(prompt.contains("sess-1"));
        assert!(prompt.contains("+hello"));
        assert!(prompt.contains("ninox brain query"));
        assert!(prompt.contains("ninox brain index"));
    }

    #[test]
    fn harvest_prompt_truncates_oversized_diffs() {
        let huge_diff = "x".repeat(MAX_INLINE_DIFF_BYTES + 500);
        let prompt = build_harvest_prompt("sess-1", &huge_diff);
        assert!(prompt.contains("truncated"));
        assert!(prompt.len() < huge_diff.len() + 2000);
    }
}
