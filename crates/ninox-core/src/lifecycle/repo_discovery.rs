//! Mechanical repo-location and repo-relationship discovery for the brain's
//! `repos/` and `relationships/` sections (see `docs/BRAIN.md`). Everything
//! here is derived from git plumbing and cheap file reads (README, Cargo.toml,
//! package.json) — no LLM call, unlike `lifecycle::brain_harvest`.

use crate::github::split_repo;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

/// Everything mechanically knowable about a repo's on-disk location and purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub name: String,
    pub path: PathBuf,
    pub remote_url: Option<String>,
    pub remote_owner: Option<String>,
    pub remote_repo: Option<String>,
    pub purpose: Option<String>,
    pub entry_points: Vec<String>,
}

/// Result of scanning a set of candidate workspace paths: one [`RepoIdentity`]
/// per distinct underlying repo (deduplicated across linked worktrees of the
/// same repo), plus any additional worktree paths observed for each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    pub repos: Vec<RepoIdentity>,
    /// repo name -> extra worktree paths observed beyond the canonical `path`.
    pub extra_worktrees: Vec<(String, Vec<PathBuf>)>,
}

/// Scan `candidates`, resolving each to its canonical repo root (the git
/// "main" worktree — see [`canonical_repo_root`]) and deriving a
/// [`RepoIdentity`] for it. Multiple candidates resolving to the same
/// canonical root collapse into a single entry; the other paths are recorded
/// as observed worktrees of that repo. Candidates that aren't inside a git
/// repo at all are silently skipped.
pub fn discover(candidates: &[PathBuf]) -> Discovery {
    let mut by_canonical: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();

    for candidate in candidates {
        if let Some(canonical) = canonical_repo_root(candidate) {
            by_canonical.entry(canonical).or_default().push(candidate.clone());
        }
    }

    let mut repos = Vec::new();
    let mut extra_worktrees = Vec::new();
    for (canonical, observed) in by_canonical {
        let Some(identity) = derive_repo_identity(&canonical) else { continue };

        let mut extras: Vec<PathBuf> =
            observed.into_iter().filter(|p| p != &canonical).collect();
        extras.sort();
        extras.dedup();
        if !extras.is_empty() {
            extra_worktrees.push((identity.name.clone(), extras));
        }
        repos.push(identity);
    }

    Discovery { repos, extra_worktrees }
}

/// Repos sharing the same remote owner/org, keyed by owner — only groups with
/// two or more members (a lone repo isn't a relationship). Ordered by owner
/// name for deterministic output.
pub fn group_by_owner(repos: &[RepoIdentity]) -> Vec<(String, Vec<String>)> {
    let mut by_owner: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for repo in repos {
        if let Some(owner) = &repo.remote_owner {
            by_owner.entry(owner.clone()).or_default().push(repo.name.clone());
        }
    }
    by_owner.into_iter().filter(|(_, names)| names.len() > 1).collect()
}

// ---------------------------------------------------------------------------
// Git plumbing
// ---------------------------------------------------------------------------

/// The canonical root of the repo `path` belongs to: `path`'s own toplevel if
/// it's a plain repo or the main git worktree, or the *main* worktree's root
/// if `path` is a linked worktree (`git worktree add`). This is what makes
/// discovery collapse a session's ephemeral `.claude/worktrees/<id>` workspace
/// down to the one repo location worth remembering, instead of writing a new
/// `repos/` entry per worktree.
///
/// Detection follows the same `.git`-is-a-file-vs-directory convention
/// `spawn_util::find_repo_root` documents: a linked worktree's `.git` is a
/// file containing `gitdir: <main>/.git/worktrees/<name>`, while a plain repo
/// or the main worktree has a real `.git` directory.
///
/// Returns `None` if `path` isn't inside a git repo at all.
fn canonical_repo_root(path: &Path) -> Option<PathBuf> {
    let toplevel = git_output(path, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))?;

    let gitfile = toplevel.join(".git");
    if gitfile.is_dir() {
        return Some(toplevel);
    }

    let content = std::fs::read_to_string(&gitfile).ok()?;
    let gitdir_line = content.lines().find_map(|l| l.strip_prefix("gitdir: "))?;
    let worktree_git_dir = {
        let p = PathBuf::from(gitdir_line.trim());
        if p.is_absolute() { p } else { toplevel.join(p) }
    };

    // `worktree_git_dir` looks like `<main-repo>/.git/worktrees/<name>` —
    // walk up to the `.git` component and take its parent as the repo root.
    let main_git_dir = worktree_git_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == ".git"))?;
    main_git_dir.parent().map(Path::to_path_buf)
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn remote_origin_url(repo_root: &Path) -> Option<String> {
    let url = git_output(repo_root, &["remote", "get-url", "origin"])?
        .trim()
        .to_string();
    if url.is_empty() { None } else { Some(url) }
}

// ---------------------------------------------------------------------------
// Identity derivation
// ---------------------------------------------------------------------------

/// Derive a [`RepoIdentity`] for a repo whose canonical root is `root`
/// (typically the output of [`canonical_repo_root`]). Returns `None` if
/// `root` doesn't look like a git repo at all.
pub fn derive_repo_identity(root: &Path) -> Option<RepoIdentity> {
    if !root.join(".git").exists() {
        return None;
    }

    let remote_url = remote_origin_url(root);
    let (remote_owner, remote_repo) = remote_url
        .as_deref()
        .and_then(split_repo)
        .map(|(o, r)| (Some(o), Some(r)))
        .unwrap_or((None, None));

    let name = remote_repo.clone().unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    Some(RepoIdentity {
        name,
        path: root.to_path_buf(),
        remote_url,
        remote_owner,
        remote_repo,
        purpose: derive_purpose(root),
        entry_points: derive_entry_points(root),
    })
}

/// A cheap purpose signal for the repo at `root`: the first non-heading line
/// of its README, falling back to `Cargo.toml`'s `[package] description` and
/// then `package.json`'s `description`. `None` if none of those exist.
fn derive_purpose(root: &Path) -> Option<String> {
    for candidate in ["README.md", "Readme.md", "readme.md", "README"] {
        if let Ok(content) = std::fs::read_to_string(root.join(candidate)) {
            if let Some(line) = first_meaningful_line(&content) {
                return Some(line);
            }
        }
    }
    cargo_toml_description(root).or_else(|| package_json_description(root))
}

/// The first non-empty, non-heading line of `content`, falling back to the
/// first non-empty line (even a heading) if every line is a heading.
fn first_meaningful_line(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().map(str::trim).collect();
    lines
        .iter()
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .or_else(|| lines.iter().find(|l| !l.is_empty()))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|l| !l.is_empty())
}

fn cargo_toml_description(root: &Path) -> Option<String> {
    let content = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = content.parse().ok()?;
    parsed
        .get("package")?
        .get("description")?
        .as_str()
        .map(str::to_string)
}

fn package_json_description(root: &Path) -> Option<String> {
    let content = std::fs::read_to_string(root.join("package.json")).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed.get("description")?.as_str().map(str::to_string)
}

/// Cheap entry-point signals for the repo at `root`: a Rust workspace's
/// member crates (globs like `crates/*` expanded one level against the
/// filesystem), else `src/main.rs`/`src/lib.rs`, else a `package.json`'s
/// `main`/`bin`. Empty if none of these apply.
fn derive_entry_points(root: &Path) -> Vec<String> {
    if let Some(points) = cargo_workspace_members(root) {
        if !points.is_empty() {
            return points;
        }
    }
    if root.join("src/main.rs").exists() {
        return vec!["src/main.rs".to_string()];
    }
    if root.join("src/lib.rs").exists() {
        return vec!["src/lib.rs".to_string()];
    }
    package_json_entry_points(root)
}

fn cargo_workspace_members(root: &Path) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = content.parse().ok()?;
    let members = parsed.get("workspace")?.get("members")?.as_array()?;

    let mut points = Vec::new();
    for member in members {
        let Some(pattern) = member.as_str() else { continue };
        if let Some(prefix) = pattern.strip_suffix("/*") {
            let Ok(entries) = std::fs::read_dir(root.join(prefix)) else { continue };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join("Cargo.toml").exists())
                .filter_map(|e| e.file_name().into_string().ok())
                .map(|n| format!("{prefix}/{n}"))
                .collect();
            names.sort();
            points.extend(names);
        } else {
            points.push(pattern.to_string());
        }
    }
    Some(points)
}

fn package_json_entry_points(root: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(root.join("package.json")) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };

    let mut points = Vec::new();
    if let Some(main) = parsed.get("main").and_then(|v| v.as_str()) {
        points.push(main.to_string());
    }
    match parsed.get("bin") {
        Some(serde_json::Value::String(s)) => points.push(s.clone()),
        Some(serde_json::Value::Object(map)) => {
            points.extend(map.values().filter_map(|v| v.as_str()).map(str::to_string))
        }
        _ => {}
    }
    points
}

// ---------------------------------------------------------------------------
// Markdown generation (pure — no I/O)
// ---------------------------------------------------------------------------

/// Brain entry id (relative path) for `repo`'s `repos/` entry.
pub fn repo_entry_id(repo_name: &str) -> String {
    format!("repos/{}.md", crate::slugify(repo_name))
}

/// Brain entry id for the `relationships/` entry recording `repo_name`'s
/// extra worktrees.
pub fn worktree_relationship_id(repo_name: &str) -> String {
    format!("relationships/{}-worktrees.md", crate::slugify(repo_name))
}

/// Brain entry id for the `relationships/` entry recording repos that share
/// remote owner `owner`.
pub fn shared_org_relationship_id(owner: &str) -> String {
    format!("relationships/{}-org.md", crate::slugify(owner))
}

/// Render `repo` as a `repos/` brain entry.
pub fn repo_entry_markdown(repo: &RepoIdentity, updated: &str) -> String {
    let mut body = format!("# {}\n\n- Path: `{}`\n", repo.name, repo.path.display());
    if let Some(url) = &repo.remote_url {
        body.push_str(&format!("- Remote: {url}\n"));
    }
    if !repo.entry_points.is_empty() {
        let points = repo
            .entry_points
            .iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(", ");
        body.push_str(&format!("- Entry points: {points}\n"));
    }
    if let Some(purpose) = &repo.purpose {
        body.push_str(&format!("\n{purpose}\n"));
    }

    format!(
        "---\ntype: repo\nname: {name}\ntags: [repo]\nrepos: [{name}]\nupdated: {updated}\n---\n\n{body}",
        name = repo.name,
    )
}

/// Render the "extra worktrees" relationship for `repo`.
pub fn worktree_relationship_markdown(repo: &RepoIdentity, worktrees: &[PathBuf], updated: &str) -> String {
    let slug = crate::slugify(&repo.name);
    let mut body = format!(
        "# {name} worktrees\n\nAdditional git worktrees observed for [[repos/{slug}|{name}]] \
         beyond its canonical location `{canonical}`:\n\n",
        name = repo.name,
        canonical = repo.path.display(),
    );
    for wt in worktrees {
        body.push_str(&format!("- `{}`\n", wt.display()));
    }

    format!(
        "---\ntype: relationship\nname: {name}-worktrees\ntags: [worktree]\nrepos: [{name}]\nupdated: {updated}\n---\n\n{body}",
        name = repo.name,
    )
}

/// Render the "shared remote owner" relationship for a group of repos owned
/// by `owner`.
pub fn shared_org_relationship_markdown(owner: &str, repo_names: &[String], updated: &str) -> String {
    let mut body = format!("# {owner} org\n\nRepos sharing the `{owner}` remote owner/org:\n\n");
    for name in repo_names {
        body.push_str(&format!("- [[repos/{}|{}]]\n", crate::slugify(name), name));
    }

    format!(
        "---\ntype: relationship\nname: {owner}-org\ntags: [org]\nrepos: [{repos}]\nupdated: {updated}\n---\n\n{body}",
        repos = repo_names.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo() -> PathBuf {
        // Canonicalized so assertions can compare directly against git's own
        // output, which resolves symlinks (e.g. macOS's /tmp -> /private/tmp).
        let dir = std::fs::canonicalize(tempdir().unwrap().keep()).unwrap();
        run_git(&dir, &["init", "-q", "-b", "main"]);
        run_git(&dir, &["config", "user.email", "test@example.com"]);
        run_git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("x.txt"), "x").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    /// A fresh, unique directory to add a worktree into — deliberately not a
    /// sibling of `repo` (which would put it directly under the shared
    /// process-wide TMPDIR and collide with other tests' `wt` worktrees).
    fn fresh_worktree_target() -> PathBuf {
        std::fs::canonicalize(tempdir().unwrap().keep()).unwrap().join("wt")
    }

    // -----------------------------------------------------------------
    // canonical_repo_root / discover
    // -----------------------------------------------------------------

    #[test]
    fn plain_repo_is_its_own_canonical_root() {
        let repo = init_repo();
        assert_eq!(canonical_repo_root(&repo), Some(repo));
    }

    #[test]
    fn non_git_dir_has_no_canonical_root() {
        let dir = tempdir().unwrap();
        assert_eq!(canonical_repo_root(dir.path()), None);
    }

    #[test]
    fn linked_worktree_resolves_to_main_repo_root() {
        let repo = init_repo();
        let worktree = fresh_worktree_target();
        run_git(
            &repo,
            &["worktree", "add", worktree.to_str().unwrap(), "-b", "feature"],
        );

        assert_eq!(canonical_repo_root(&worktree), Some(repo.clone()));
    }

    #[test]
    fn discover_dedupes_main_repo_and_its_worktree_into_one_entry() {
        let repo = init_repo();
        run_git(&repo, &["remote", "add", "origin", "git@github.com:acme/widget.git"]);
        let worktree = fresh_worktree_target();
        run_git(
            &repo,
            &["worktree", "add", worktree.to_str().unwrap(), "-b", "feature"],
        );

        let discovery = discover(&[repo.clone(), worktree.clone()]);

        assert_eq!(discovery.repos.len(), 1, "one repo, not two: {:?}", discovery.repos);
        assert_eq!(discovery.repos[0].name, "widget");
        assert_eq!(discovery.repos[0].path, repo);

        assert_eq!(discovery.extra_worktrees.len(), 1);
        assert_eq!(discovery.extra_worktrees[0].0, "widget");
        assert_eq!(discovery.extra_worktrees[0].1, vec![worktree]);
    }

    #[test]
    fn discover_skips_non_git_candidates() {
        let not_a_repo = tempdir().unwrap();
        let discovery = discover(&[not_a_repo.path().to_path_buf()]);
        assert!(discovery.repos.is_empty());
        assert!(discovery.extra_worktrees.is_empty());
    }

    // -----------------------------------------------------------------
    // derive_repo_identity
    // -----------------------------------------------------------------

    #[test]
    fn identity_falls_back_to_dir_name_without_a_remote() {
        let repo = init_repo();
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.name, repo.file_name().unwrap().to_string_lossy());
        assert!(identity.remote_url.is_none());
        assert!(identity.remote_owner.is_none());
    }

    #[test]
    fn identity_prefers_remote_repo_name_over_dir_name() {
        let repo = init_repo();
        run_git(&repo, &["remote", "add", "origin", "https://github.com/acme/widget.git"]);
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.name, "widget");
        assert_eq!(identity.remote_owner.as_deref(), Some("acme"));
        assert_eq!(identity.remote_repo.as_deref(), Some("widget"));
        assert_eq!(identity.remote_url.as_deref(), Some("https://github.com/acme/widget.git"));
    }

    #[test]
    fn purpose_prefers_first_non_heading_readme_line() {
        let repo = init_repo();
        std::fs::write(repo.join("README.md"), "# Widget\n\nA gadget that widgets things.\n").unwrap();
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.purpose.as_deref(), Some("A gadget that widgets things."));
    }

    #[test]
    fn purpose_falls_back_to_heading_when_readme_is_only_a_title() {
        let repo = init_repo();
        std::fs::write(repo.join("README.md"), "# Widget\n").unwrap();
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.purpose.as_deref(), Some("Widget"));
    }

    #[test]
    fn purpose_falls_back_to_cargo_toml_description() {
        let repo = init_repo();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"widget\"\ndescription = \"Widgets, but fast\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.purpose.as_deref(), Some("Widgets, but fast"));
    }

    #[test]
    fn purpose_falls_back_to_package_json_description() {
        let repo = init_repo();
        std::fs::write(
            repo.join("package.json"),
            r#"{"name": "widget", "description": "A JS widget"}"#,
        )
        .unwrap();
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.purpose.as_deref(), Some("A JS widget"));
    }

    #[test]
    fn entry_points_expand_workspace_glob_members() {
        let repo = init_repo();
        std::fs::write(repo.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").unwrap();
        std::fs::create_dir_all(repo.join("crates/foo")).unwrap();
        std::fs::write(repo.join("crates/foo/Cargo.toml"), "[package]\nname=\"foo\"\n").unwrap();
        std::fs::create_dir_all(repo.join("crates/bar")).unwrap();
        std::fs::write(repo.join("crates/bar/Cargo.toml"), "[package]\nname=\"bar\"\n").unwrap();
        // Not a crate — no Cargo.toml — must not appear as an entry point.
        std::fs::create_dir_all(repo.join("crates/not-a-crate")).unwrap();

        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.entry_points, vec!["crates/bar".to_string(), "crates/foo".to_string()]);
    }

    #[test]
    fn entry_points_fall_back_to_src_main_rs() {
        let repo = init_repo();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        let identity = derive_repo_identity(&repo).unwrap();
        assert_eq!(identity.entry_points, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn entry_points_fall_back_to_package_json_main_and_bin() {
        let repo = init_repo();
        std::fs::write(
            repo.join("package.json"),
            r#"{"name": "widget", "main": "index.js", "bin": {"widget-cli": "bin/cli.js"}}"#,
        )
        .unwrap();
        let identity = derive_repo_identity(&repo).unwrap();
        assert!(identity.entry_points.contains(&"index.js".to_string()));
        assert!(identity.entry_points.contains(&"bin/cli.js".to_string()));
    }

    // -----------------------------------------------------------------
    // group_by_owner
    // -----------------------------------------------------------------

    fn identity(name: &str, owner: Option<&str>) -> RepoIdentity {
        RepoIdentity {
            name: name.to_string(),
            path: PathBuf::from(format!("/repos/{name}")),
            remote_url: None,
            remote_owner: owner.map(str::to_string),
            remote_repo: Some(name.to_string()),
            purpose: None,
            entry_points: Vec::new(),
        }
    }

    #[test]
    fn group_by_owner_only_reports_groups_with_two_or_more() {
        let repos = vec![
            identity("widget", Some("acme")),
            identity("gadget", Some("acme")),
            identity("lonely", Some("solo-corp")),
            identity("no-remote", None),
        ];
        let groups = group_by_owner(&repos);
        assert_eq!(groups, vec![("acme".to_string(), vec!["widget".to_string(), "gadget".to_string()])]);
    }

    // -----------------------------------------------------------------
    // Markdown generation
    // -----------------------------------------------------------------

    #[test]
    fn repo_entry_markdown_includes_frontmatter_and_facts() {
        let repo = RepoIdentity {
            name: "widget".to_string(),
            path: PathBuf::from("/home/x/widget"),
            remote_url: Some("git@github.com:acme/widget.git".to_string()),
            remote_owner: Some("acme".to_string()),
            remote_repo: Some("widget".to_string()),
            purpose: Some("Widgets, but fast".to_string()),
            entry_points: vec!["src/main.rs".to_string()],
        };
        let md = repo_entry_markdown(&repo, "2026-07-07");

        assert!(md.starts_with("---\n"));
        assert!(md.contains("type: repo"));
        assert!(md.contains("name: widget"));
        assert!(md.contains("repos: [widget]"));
        assert!(md.contains("updated: 2026-07-07"));
        assert!(md.contains("/home/x/widget"));
        assert!(md.contains("git@github.com:acme/widget.git"));
        assert!(md.contains("src/main.rs"));
        assert!(md.contains("Widgets, but fast"));
    }

    #[test]
    fn repo_entry_id_slugifies_name() {
        assert_eq!(repo_entry_id("Ninox Core"), "repos/ninox-core.md");
    }

    #[test]
    fn worktree_relationship_markdown_lists_every_worktree() {
        let repo = identity("widget", Some("acme"));
        let worktrees = vec![PathBuf::from("/w/wt1"), PathBuf::from("/w/wt2")];
        let md = worktree_relationship_markdown(&repo, &worktrees, "2026-07-07");

        assert!(md.contains("type: relationship"));
        assert!(md.contains("repos: [widget]"));
        assert!(md.contains("[[repos/widget|widget]]"));
        assert!(md.contains("/w/wt1"));
        assert!(md.contains("/w/wt2"));
    }

    #[test]
    fn shared_org_relationship_markdown_links_every_repo() {
        let md = shared_org_relationship_markdown(
            "acme",
            &["widget".to_string(), "gadget".to_string()],
            "2026-07-07",
        );
        assert!(md.contains("type: relationship"));
        assert!(md.contains("repos: [widget, gadget]"));
        assert!(md.contains("[[repos/widget|widget]]"));
        assert!(md.contains("[[repos/gadget|gadget]]"));
    }
}
