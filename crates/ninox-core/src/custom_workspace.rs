//! An orchestrator running in a folder the user chose (`--workspace`).
//!
//! That folder is the user's: ninox never writes `CLAUDE.md`, `AGENTS.md` or
//! `.claude/settings.json` there. What it adds is confined to
//! `.claude/settings.local.json` (ninox's hook/statusLine entries merged
//! beside the user's) and `.claude/skills/ninox-*` (marked, so one the user
//! wrote is never overwritten). The role text an orchestrator root puts in
//! AGENTS.md reaches the agent as an appended system prompt instead, and the
//! hook scripts stay in ninox's own orchestrator root.

use std::path::{Path, PathBuf};

use crate::capabilities::{self, Audience, Capability};
use crate::config::AppConfig;

/// First line after a managed skill's frontmatter; only a skill carrying it
/// is ever rewritten or removed.
pub const MANAGED_MARKER: &str = "<!-- ninox-managed -->";

const SKILL_PREFIX: &str = "ninox-";

pub fn skill_dir_name(name: &str) -> String {
    if name.starts_with(SKILL_PREFIX) { name.to_string() } else { format!("{SKILL_PREFIX}{name}") }
}

/// Resolve and check a `--workspace` value: `~` expanded, must exist and be
/// a directory, canonicalised, and not inside ninox's own orchestrator root
/// (which already is the default workspace).
pub fn resolve(input: &str, orchestrator_root: &Path) -> anyhow::Result<PathBuf> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "--workspace needs a directory");
    let expanded = match input.strip_prefix("~/").or_else(|| (input == "~").then_some("")) {
        Some(rest) => dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot expand ~: no home directory"))?.join(rest),
        None => PathBuf::from(input),
    };
    anyhow::ensure!(expanded.exists(), "workspace folder {} does not exist", expanded.display());
    anyhow::ensure!(expanded.is_dir(), "workspace {} is not a directory", expanded.display());
    let dir = expanded.canonicalize()?;
    let home = dirs::home_dir().and_then(|h| h.canonicalize().ok());
    anyhow::ensure!(
        dir.parent().is_some() && home.as_deref() != Some(dir.as_path()),
        "{} is too broad to be a workspace; pick a project folder",
        dir.display()
    );
    let root = orchestrator_root.canonicalize().unwrap_or_else(|_| orchestrator_root.to_path_buf());
    anyhow::ensure!(
        !dir.starts_with(&root),
        "{} is inside ninox's own orchestrator folder; omit --workspace to use it",
        dir.display()
    );
    Ok(dir)
}

/// The role text appended to the launch command in place of AGENTS.md.
pub fn role_prompt(config: &AppConfig) -> String {
    let caps: Vec<&Capability> = capabilities::enabled_for(Audience::Orchestrator, config).collect();
    crate::orchestrator_root::role_text(
        &caps,
        &|name| Path::new(".claude/skills").join(skill_dir_name(name)).join("SKILL.md"),
        "ninox",
    )
}

/// Rewrites a rendered skill for its namespaced install: frontmatter `name:`
/// and every `<name> skill` cross-reference to the `ninox-` name, and the
/// managed marker after the frontmatter.
pub fn namespace_skill(rendered: &str, own: &str, all: &[&str]) -> String {
    let mut out = rendered.replacen(&format!("name: {own}\n"), &format!("name: {}\n", skill_dir_name(own)), 1);
    for name in all {
        let new = skill_dir_name(name);
        out = out.replace(&format!("`{name}` skill"), &format!("`{new}` skill"));
        let plain = format!("{name} skill");
        let mut rebuilt = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(&plain) {
            let before = rest[..i].chars().next_back();
            rebuilt.push_str(&rest[..i]);
            if matches!(before, Some(' ' | '(' | '\n')) {
                rebuilt.push_str(&format!("{new} skill"));
            } else {
                rebuilt.push_str(&plain);
            }
            rest = &rest[i + plain.len()..];
        }
        rebuilt.push_str(rest);
        out = rebuilt;
    }
    match out.strip_prefix("---\n").and_then(|b| b.find("\n---\n").map(|i| i + 4 + 5)) {
        Some(end) => format!("{}{MANAGED_MARKER}\n{}", &out[..end], &out[end..]),
        None => format!("{MANAGED_MARKER}\n{out}"),
    }
}

#[derive(Debug, Default)]
pub struct Installed {
    /// Non-fatal problems for the spawner to show.
    pub warnings: Vec<String>,
}

fn is_managed(skill_dir: &Path) -> bool {
    std::fs::read_to_string(skill_dir.join("SKILL.md")).is_ok_and(|s| s.contains(MANAGED_MARKER))
}

fn single_quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Seeds `workspace` for an orchestrator running there; idempotent, safe to
/// call on every launch, restart and server start. `root` is ninox's
/// orchestrator root, whose `.claude/` holds the hook scripts the merged hooks
/// call. Failures are reported in [`Installed::warnings`], never fatal.
pub async fn install(
    workspace: &Path,
    root: &Path,
    config: &AppConfig,
    harness: &str,
    ninox_bin: &str,
    config_path: &str,
) -> Installed {
    let mut report = Installed::default();
    // Never create the user's folder: a vanished one is reported, not rebuilt.
    if !workspace.is_dir() {
        report.warnings.push(format!("workspace folder {} does not exist", workspace.display()));
        return report;
    }
    let root_claude = root.join(".claude");
    if let Err(e) = crate::orchestrator_root::write_guard_scripts(&root_claude, config).await {
        report.warnings.push(format!("could not write ninox's hook scripts in {}: {e}", root_claude.display()));
    }

    let mut skill_dirs = vec![PathBuf::from(".claude/skills")];
    skill_dirs.extend(config.registry().spec(harness).extra_skills_dir.map(PathBuf::from));
    let caps: Vec<&Capability> = capabilities::enabled_for(Audience::Orchestrator, config)
        .filter(|c| c.orchestrator_md.is_some())
        .collect();
    let all_names: Vec<&str> = capabilities::for_audience(Audience::Orchestrator).map(|c| c.name).collect();
    for rel in &skill_dirs {
        let base = workspace.join(rel);
        for cap in &caps {
            let md = capabilities::render(cap.orchestrator_md.unwrap_or_default(), ninox_bin, config_path);
            let body = namespace_skill(&md, cap.name, &all_names);
            let dir = base.join(skill_dir_name(cap.name));
            if dir.exists() && !is_managed(&dir) {
                report.warnings.push(format!(
                    "{} already exists and isn't ninox's; left untouched (the {} skill is unavailable)",
                    dir.display(), cap.name
                ));
                continue;
            }
            let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("SKILL.md"), body));
            if let Err(e) = write {
                report.warnings.push(format!("could not write {}: {e}", dir.display()));
            }
        }
        prune_stale_skills(&base, &caps);
        git_exclude(workspace, &format!("{}/{SKILL_PREFIX}*/", rel.display()));
    }
    git_exclude(workspace, ".claude/settings.local.json");

    let claude_dir = workspace.join(".claude");
    if git_tracked(workspace, ".claude/settings.local.json") {
        report.warnings.push(format!(
            "{} is tracked by git; ninox's hooks and statusLine were not installed there",
            claude_dir.join("settings.local.json").display()
        ));
        return report;
    }
    let scripts = root_claude.clone();
    let mut entries = crate::orchestrator_root::orchestrator_settings(config, ninox_bin, &|script| {
        format!("node {}", single_quoted(&scripts.join(script)))
    });
    if crate::claude_settings::user_status_line(&claude_dir) {
        if let Some(obj) = entries.as_object_mut() {
            obj.remove("statusLine");
        }
        report.warnings.push(format!(
            "{} sets its own statusLine, which ninox keeps: context %, cost and automatic reset-from-plan \
             are unavailable for this orchestrator",
            claude_dir.display()
        ));
    }
    crate::claude_settings::without_legacy_duplicates(&claude_dir, &mut entries);
    if let Err(e) = crate::claude_settings::install_local(&claude_dir, &entries) {
        report.warnings.push(format!("ninox's hooks were not installed: {e:#}"));
    }
    report
}

/// Removes ninox-managed skills that the registry no longer seeds here (a
/// capability since disabled).
fn prune_stale_skills(base: &Path, current: &[&Capability]) {
    let keep: Vec<String> = current.iter().map(|c| skill_dir_name(c.name)).collect();
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(SKILL_PREFIX) && !keep.contains(&name) && is_managed(&entry.path()) {
            remove_managed_skill(&entry.path());
        }
    }
}

fn remove_managed_skill(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("SKILL.md"));
    let _ = std::fs::remove_dir(dir);
}

/// Takes ninox back out of `workspace`: its hook and statusLine entries in
/// `settings.local.json` (a `crossSessionInbound` value stays) and its marked skills. Never touches anything else, and never removes the
/// folder.
pub fn uninstall(workspace: &Path) -> anyhow::Result<()> {
    if let Err(e) = crate::claude_settings::strip_local(&workspace.join(".claude")) {
        tracing::warn!("custom workspace {}: {e:#}", workspace.display());
    }
    for rel in [".claude/skills", ".agents/skills"] {
        let base = workspace.join(rel);
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(SKILL_PREFIX) && is_managed(&entry.path()) {
                remove_managed_skill(&entry.path());
            }
        }
    }
    Ok(())
}

fn git_tracked(workspace: &Path, rel: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "--error-unmatch", "--", rel])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Appends `pattern` (relative to the workspace) to the repo's
/// `info/exclude` unless the line is already there. Never rewrites the file;
/// silently does nothing outside a git repo. `--git-path` resolves a
/// worktree's `.git` file to the shared git dir.
fn git_exclude(workspace: &Path, pattern: &str) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let (Some(exclude), Some(prefix)) = (git(&["rev-parse", "--git-path", "info/exclude"]), git(&["rev-parse", "--show-prefix"]))
    else {
        return;
    };
    let exclude = if Path::new(&exclude).is_absolute() { PathBuf::from(exclude) } else { workspace.join(exclude) };
    let line = format!("/{prefix}{pattern}");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return;
    }
    if let Some(dir) = exclude.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
    use std::io::Write as _;
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .and_then(|mut f| writeln!(f, "{sep}{line}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=t", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    const USER_SETTINGS: &str = r#"{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "bin/sync pull"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "bin/sync push"}]}]
  },
  "outputStyle": "terse"
}"#;

    /// A repo shaped like the one the feature was asked for: tracked
    /// settings.json with hooks, a CLAUDE.md, and a .gitignore covering
    /// settings.local.json and skills/, plus a key in settings.local.json.
    fn fake_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        std::fs::create_dir_all(p.join(".claude")).unwrap();
        std::fs::write(p.join(".claude/settings.json"), USER_SETTINGS).unwrap();
        std::fs::write(p.join("CLAUDE.md"), "# my brain\n").unwrap();
        std::fs::write(p.join(".gitignore"), ".claude/settings.local.json\n.claude/skills/\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "-m", "init"]);
        std::fs::write(
            p.join(".claude/settings.local.json"),
            json!({"permissions": {"allow": ["Bash(ls)"]}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "mine --local"}]}]}}).to_string(),
        )
        .unwrap();
        dir
    }

    async fn install_in(ws: &Path, root: &Path) -> Installed {
        install(ws, root, &AppConfig::default(), "claude-code", "/opt/ninox/bin/ninox", "/cfg.toml").await
    }

    fn local(ws: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(ws.join(".claude/settings.local.json")).unwrap()).unwrap()
    }

    fn commands(v: &Value, event: &str) -> Vec<String> {
        v["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn install_keeps_the_repo_clean_and_the_users_files_byte_identical() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        let (claude_md, settings) = (
            std::fs::read(repo.path().join("CLAUDE.md")).unwrap(),
            std::fs::read(repo.path().join(".claude/settings.json")).unwrap(),
        );
        let report = install_in(repo.path(), root.path()).await;
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        assert_eq!(git(repo.path(), &["status", "--porcelain"]), "");
        assert_eq!(std::fs::read(repo.path().join("CLAUDE.md")).unwrap(), claude_md);
        assert_eq!(std::fs::read(repo.path().join(".claude/settings.json")).unwrap(), settings);
        assert!(!repo.path().join("AGENTS.md").exists());
        // Hook scripts live in ninox's root, not in the repo.
        assert!(!repo.path().join(".claude/subagent-blocker.cjs").exists());
        assert!(root.path().join(".claude/subagent-blocker.cjs").exists());
    }

    #[tokio::test]
    async fn install_merges_beside_user_hooks_and_is_idempotent() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        install_in(repo.path(), root.path()).await;
        let first = std::fs::read_to_string(repo.path().join(".claude/settings.local.json")).unwrap();
        install_in(repo.path(), root.path()).await;
        assert_eq!(first, std::fs::read_to_string(repo.path().join(".claude/settings.local.json")).unwrap());

        let v = local(repo.path());
        assert_eq!(v["permissions"]["allow"], json!(["Bash(ls)"]));
        let stop = commands(&v, "Stop");
        assert_eq!(stop[0], "mine --local", "the user's hook keeps its place");
        assert_eq!(stop.iter().filter(|c| claude_settings_is_ninox(c)).count(), 1, "{stop:?}");
        assert!(v["statusLine"]["command"].as_str().unwrap().ends_with("ninox' statusline"));
        let blocker = commands(&v, "PreToolUse").into_iter().find(|c| c.contains("subagent-blocker")).unwrap();
        assert!(blocker.contains(&root.path().display().to_string()), "{blocker}");
    }

    fn claude_settings_is_ninox(cmd: &str) -> bool {
        crate::claude_settings::is_ninox_command(cmd)
    }

    #[tokio::test]
    async fn refresh_replaces_only_ninox_entries_when_the_binary_moves() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        install_in(repo.path(), root.path()).await;
        install(repo.path(), root.path(), &AppConfig::default(), "claude-code", "/new/place/ninox", "/cfg.toml").await;
        let v = local(repo.path());
        let stop = commands(&v, "Stop");
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert!(stop[1].contains("/new/place/ninox"), "{stop:?}");
        assert!(!serde_json::to_string(&v).unwrap().contains("/opt/ninox/bin"));
    }

    #[tokio::test]
    async fn users_status_line_is_kept_with_a_plain_warning() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        let mut settings: Value = serde_json::from_str(USER_SETTINGS).unwrap();
        settings["statusLine"] = json!({"type": "command", "command": "my-line"});
        std::fs::write(repo.path().join(".claude/settings.json"), settings.to_string()).unwrap();
        let report = install_in(repo.path(), root.path()).await;
        assert!(local(repo.path()).get("statusLine").is_none());
        assert!(report.warnings.iter().any(|w| w.contains("context %") && w.contains("reset-from-plan")), "{:?}", report.warnings);
    }

    #[tokio::test]
    async fn skills_are_namespaced_marked_and_never_overwrite_a_users() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        let mine = repo.path().join(".claude/skills/ninox-brain");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("SKILL.md"), "mine\n").unwrap();
        let report = install_in(repo.path(), root.path()).await;

        assert_eq!(std::fs::read_to_string(mine.join("SKILL.md")).unwrap(), "mine\n");
        assert!(report.warnings.iter().any(|w| w.contains("ninox-brain") && w.contains("isn't ninox's")), "{:?}", report.warnings);
        let spawn = std::fs::read_to_string(repo.path().join(".claude/skills/ninox-spawn-worker/SKILL.md")).unwrap();
        assert!(spawn.starts_with("---\n") && spawn.contains("name: ninox-spawn-worker\n") && spawn.contains(MANAGED_MARKER));
        assert!(!spawn.contains("name: spawn-worker"));
        assert!(!repo.path().join(".claude/skills/spawn-worker").exists());
        assert_eq!(capabilities::description(&spawn), capabilities::description(capabilities::REGISTRY.iter()
            .find(|c| c.name == "spawn-worker").unwrap().orchestrator_md.unwrap()));
    }

    #[test]
    fn cross_references_are_renamed_in_every_skill() {
        let all: Vec<&str> = capabilities::for_audience(Audience::Orchestrator).map(|c| c.name).collect();
        for cap in capabilities::for_audience(Audience::Orchestrator) {
            let body = namespace_skill(cap.orchestrator_md.unwrap(), cap.name, &all);
            for n in &all {
                assert!(!body.contains(&format!("`{n}` skill")), "{}: `{n}` skill", cap.name);
                for lead in [' ', '(', '\n'] {
                    assert!(!body.contains(&format!("{lead}{n} skill")), "{}: {n} skill", cap.name);
                }
            }
        }
    }

    #[test]
    fn role_prompt_points_at_the_namespaced_skills() {
        let role = role_prompt(&AppConfig::default());
        assert!(role.contains(".claude/skills/ninox-spawn-worker/SKILL.md"), "{role}");
        assert!(role.contains("ninox capabilities --orchestrator"));
        assert!(!role.contains(".claude/skills/spawn-worker"));
    }

    #[tokio::test]
    async fn uninstall_removes_only_what_ninox_added() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        let before = local(repo.path());
        let mine = repo.path().join(".claude/skills/my-skill");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("SKILL.md"), "mine\n").unwrap();
        install_in(repo.path(), root.path()).await;
        uninstall(repo.path()).unwrap();

        // `crossSessionInbound: accept` stays: it can't be told from a value
        // the user set themselves, and the launch command carries it anyway.
        let mut after = local(repo.path());
        assert_eq!(after.as_object_mut().unwrap().remove("crossSessionInbound"), Some(json!("accept")));
        assert_eq!(after, before);
        assert!(mine.join("SKILL.md").exists());
        assert!(!repo.path().join(".claude/skills/ninox-spawn-worker").exists());
        assert!(repo.path().join("CLAUDE.md").exists());
        assert_eq!(git(repo.path(), &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn exclude_lines_are_appended_once_and_a_worktrees_git_file_resolves() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        let wt = repo.path().join(".claude/worktrees/feat");
        git(repo.path(), &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()]);
        assert!(wt.join(".git").is_file());
        install_in(&wt, root.path()).await;
        install_in(&wt, root.path()).await;
        let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("/.claude/settings.local.json").count(), 1, "{exclude}");
        assert!(exclude.contains("/.claude/skills/ninox-*/"), "{exclude}");
    }

    #[tokio::test]
    async fn a_plain_folder_without_git_installs_quietly() {
        let (dir, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let report = install_in(dir.path(), root.path()).await;
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(dir.path().join(".claude/settings.local.json").exists());
    }

    #[tokio::test]
    async fn a_missing_folder_is_reported_never_created() {
        let (dir, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let gone = dir.path().join("gone");
        let report = install_in(&gone, root.path()).await;
        assert!(report.warnings.iter().any(|w| w.contains("does not exist")), "{:?}", report.warnings);
        assert!(!gone.exists());
    }

    #[tokio::test]
    async fn a_tracked_settings_local_json_is_not_rewritten() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        git(repo.path(), &["add", "-f", ".claude/settings.local.json"]);
        git(repo.path(), &["commit", "-q", "-m", "track local"]);
        let before = std::fs::read(repo.path().join(".claude/settings.local.json")).unwrap();
        let report = install_in(repo.path(), root.path()).await;
        assert!(report.warnings.iter().any(|w| w.contains("tracked by git")), "{:?}", report.warnings);
        assert_eq!(std::fs::read(repo.path().join(".claude/settings.local.json")).unwrap(), before);
        assert_eq!(git(repo.path(), &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn an_unchanged_merge_leaves_the_users_file_bytes_alone() {
        let (repo, root) = (fake_repo(), tempfile::tempdir().unwrap());
        install_in(repo.path(), root.path()).await;
        let path = repo.path().join(".claude/settings.local.json");
        // The user reformats it; a refresh that changes nothing must not rewrite it.
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let compact = serde_json::to_string(&v).unwrap();
        std::fs::write(&path, &compact).unwrap();
        install_in(repo.path(), root.path()).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), compact);
    }

    #[test]
    fn resolve_rejects_home_and_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve("/", root.path()).unwrap_err().to_string().contains("too broad"));
        if let Some(home) = dirs::home_dir() {
            assert!(resolve(home.to_str().unwrap(), root.path()).unwrap_err().to_string().contains("too broad"));
        }
    }

    #[test]
    fn resolve_checks_the_folder() {
        let (dir, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert_eq!(resolve(dir.path().to_str().unwrap(), root.path()).unwrap(), dir.path().canonicalize().unwrap());
        let missing = dir.path().join("nope");
        let err = resolve(missing.to_str().unwrap(), root.path()).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(resolve(file.to_str().unwrap(), root.path()).unwrap_err().to_string().contains("not a directory"));
        let inside = root.path().join("orch");
        std::fs::create_dir(&inside).unwrap();
        assert!(resolve(inside.to_str().unwrap(), root.path()).unwrap_err().to_string().contains("orchestrator folder"));
    }
}
