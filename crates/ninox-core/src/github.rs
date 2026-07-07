use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{header, Client};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PrStatus {
    pub merged:    bool,
    pub state:     String,   // "open" | "closed"
    pub mergeable: Option<bool>,
    pub title:     String,
    pub number:    u64,
    pub head_sha:  String,
}

/// A PR found by searching for an existing head branch (`find_open_pr_for_branch`),
/// independent of any metadata the `gh` wrapper hook may or may not have recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct PrRef {
    pub number: u64,
    pub url:    String,
}

#[derive(Debug, Clone)]
pub struct CheckRun {
    pub name:        String,
    pub status:      String,      // "queued" | "in_progress" | "completed"
    pub conclusion:  Option<String>, // "success" | "failure" | "neutral" | ...
}

#[derive(Debug, Clone)]
pub struct ReviewThread {
    pub id:     i64,
    pub author: String,
    pub body:   String,
    pub path:   Option<String>,
    pub line:   Option<u32>,
    pub state:  String,  // "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED"
}

// ---------------------------------------------------------------------------
// Internal API response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhPrHead {
    sha: String,
}

#[derive(Deserialize)]
struct GhPr {
    number:    u64,
    title:     String,
    state:     String,
    merged:    bool,
    mergeable: Option<bool>,
    head:      GhPrHead,
}

#[derive(Deserialize)]
struct GhCheckRunsResponse {
    check_runs: Vec<GhCheckRun>,
}

#[derive(Deserialize)]
struct GhCheckRun {
    name:       String,
    status:     String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct GhReview {
    id:   i64,
    user: GhUser,
    body: String,
    state: String,
}

#[derive(Deserialize)]
struct GhReviewComment {
    id:   i64,
    user: GhUser,
    body: String,
    path: Option<String>,
    line: Option<u32>,
}

#[derive(Deserialize)]
struct GhUser { login: String }

/// Shape returned by the PR *list* endpoint (`GET .../pulls?head=...`) — much
/// thinner than the single-PR shape (`GhPr`): no `merged`/`mergeable`, so it
/// cannot reuse `GhPr`.
#[derive(Deserialize)]
struct GhPrListItem {
    number:   u64,
    html_url: String,
}

// ---------------------------------------------------------------------------
// Trait — allows the poller to be driven by a fake in tests, without any
// network access.
// ---------------------------------------------------------------------------

#[async_trait]
pub trait GithubApi: Send + Sync {
    async fn get_pr_status(&self, owner: &str, repo: &str, pr_number: u64) -> Result<PrStatus>;
    async fn get_ci_checks(&self, owner: &str, repo: &str, head_sha: &str) -> Result<Vec<CheckRun>>;
    async fn get_review_threads(&self, owner: &str, repo: &str, pr_number: u64) -> Result<Vec<ReviewThread>>;
    /// Find an open PR whose head branch is `branch`, independent of any
    /// metadata the `gh` wrapper hook may or may not have recorded — the
    /// active fallback for PRs created outside the wrapped `gh pr create`.
    async fn find_open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> Result<Option<PrRef>>;
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GitHubClient {
    http:  Client,
    token: String,
}

impl GitHubClient {
    pub fn new(token: String) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        let http = Client::builder()
            .user_agent("ninox/0.1")
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, token })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

#[async_trait]
impl GithubApi for GitHubClient {
    async fn get_pr_status(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<PrStatus> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}"
        );
        let gh: GhPr = self
            .http
            .get(&url)
            .header(header::AUTHORIZATION, self.auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(PrStatus {
            merged:    gh.merged,
            state:     gh.state,
            mergeable: gh.mergeable,
            title:     gh.title,
            number:    gh.number,
            head_sha:  gh.head.sha,
        })
    }

    async fn get_ci_checks(
        &self,
        owner: &str,
        repo: &str,
        head_sha: &str,
    ) -> Result<Vec<CheckRun>> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/commits/{head_sha}/check-runs?per_page=100"
        );
        let resp: GhCheckRunsResponse = self
            .http
            .get(&url)
            .header(header::AUTHORIZATION, self.auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.check_runs.into_iter().map(|r| CheckRun {
            name:       r.name,
            status:     r.status,
            conclusion: r.conclusion,
        }).collect())
    }

    async fn get_review_threads(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<ReviewThread>> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}/reviews?per_page=100"
        );
        let reviews: Vec<GhReview> = self
            .http
            .get(&url)
            .header(header::AUTHORIZATION, self.auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // Also fetch inline review comments
        let comments_url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}/comments?per_page=100"
        );
        let comments: Vec<GhReviewComment> = self
            .http
            .get(&comments_url)
            .header(header::AUTHORIZATION, self.auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut threads: Vec<ReviewThread> = reviews.into_iter().map(|r| ReviewThread {
            id:     r.id,
            author: r.user.login,
            body:   r.body,
            path:   None,
            line:   None,
            state:  r.state,
        }).collect();

        for c in comments {
            threads.push(ReviewThread {
                id:     c.id,
                author: c.user.login,
                body:   c.body,
                path:   c.path,
                line:   c.line,
                state:  "COMMENTED".to_string(),
            });
        }

        Ok(threads)
    }

    async fn find_open_pr_for_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<PrRef>> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open&per_page=5"
        );
        let items: Vec<GhPrListItem> = self
            .http
            .get(&url)
            .header(header::AUTHORIZATION, self.auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(items.into_iter().next().map(|p| PrRef { number: p.number, url: p.html_url }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a git remote URL or bare slug into (owner, repo). Handles
/// "owner/repo", "https://github.com/owner/repo(.git)",
/// "ssh://[user@]host/owner/repo(.git)", and scp-like SSH syntax
/// "[user@]host:owner/repo(.git)" — including a *custom* ssh config host
/// alias (e.g. `git@github.com-work:owner/repo.git`), which multi-remote
/// setups (a personal `origin` alongside an internal mirror reached via an
/// aliased SSH host) rely on and which a fixed `github.com` prefix can't
/// recognize.
pub fn split_repo(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let tail = if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
        rest.trim_start_matches("github.com/")
    } else if let Some(rest) = s.strip_prefix("ssh://") {
        rest.split_once('/').map(|(_host, r)| r).unwrap_or(rest)
    } else if let Some((_host, rest)) = s.split_once(':') {
        // scp-like syntax: the host is whatever precedes the colon —
        // deliberately not matched against a literal `github.com`.
        rest
    } else {
        s.trim_start_matches("github.com/")
    };
    let mut parts = tail.trim_start_matches('/').splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() { return None; }
    Some((owner, repo))
}

/// Every configured git remote's GitHub repo slug (`owner/repo`) for
/// `workspace`, `origin` first (the common case, tried with no extra
/// requests) then any other remote in `git remote` order. Repos here
/// routinely carry a personal `origin` alongside an internal mirror remote —
/// PR detection must not assume a PR always lives against `origin`.
/// Returns an empty `Vec` if `workspace` isn't a git repo or has no remotes.
pub fn candidate_repos(workspace: &str) -> Vec<String> {
    let Ok(output) = std::process::Command::new("git")
        .args(["-C", workspace, "remote"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    names.sort_by_key(|n| if n == "origin" { 0 } else { 1 });

    let mut repos = Vec::new();
    for name in names {
        let Ok(out) = std::process::Command::new("git")
            .args(["-C", workspace, "remote", "get-url", &name])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Some((owner, repo)) = split_repo(&url) {
            let slug = format!("{owner}/{repo}");
            if !repos.contains(&slug) {
                repos.push(slug);
            }
        }
    }
    repos
}

/// The branch currently checked out in `workspace`. `None` on detached HEAD
/// or if `workspace` isn't a git repo.
pub fn current_branch(workspace: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", workspace, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }
    Some(branch)
}

/// Resolve GitHub token: config value → GITHUB_TOKEN env → `gh auth token`.
pub fn resolve_token(config_token: Option<String>) -> Option<String> {
    config_token
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .or_else(|| {
            std::process::Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_owner_from_url() {
        let (owner, repo) = split_repo("Made-by-Moonlight/Athene").unwrap();
        assert_eq!(owner, "Made-by-Moonlight");
        assert_eq!(repo, "Athene");
    }

    #[test]
    fn parse_repo_owner_strips_github_prefix() {
        let (owner, repo) = split_repo("github.com/Made-by-Moonlight/Athene").unwrap();
        assert_eq!(owner, "Made-by-Moonlight");
        assert_eq!(repo, "Athene");
    }

    #[test]
    fn invalid_repo_returns_none() {
        assert!(split_repo("notarepo").is_none());
    }

    #[test]
    fn parse_repo_owner_from_ssh_scp_syntax() {
        let (owner, repo) = split_repo("git@github.com:Made-by-Moonlight/Athene.git").unwrap();
        assert_eq!(owner, "Made-by-Moonlight");
        assert_eq!(repo, "Athene");
    }

    /// A custom ssh config host alias (e.g. `Host github.com-work` pointing
    /// at a different account/key) is exactly how this environment's
    /// internal mirror remote is configured — the host must be recognized
    /// structurally (anything before the `:`), not by matching a literal
    /// `github.com`.
    #[test]
    fn parse_repo_owner_from_ssh_scp_syntax_with_custom_host_alias() {
        let (owner, repo) = split_repo("git@github.com-synthesia:Synthesia-Technologies/ninox.git").unwrap();
        assert_eq!(owner, "Synthesia-Technologies");
        assert_eq!(repo, "ninox");
    }

    #[test]
    fn parse_repo_owner_from_ssh_url_syntax() {
        let (owner, repo) = split_repo("ssh://git@github.com/Made-by-Moonlight/Athene.git").unwrap();
        assert_eq!(owner, "Made-by-Moonlight");
        assert_eq!(repo, "Athene");
    }

    #[test]
    fn parse_repo_owner_strips_git_suffix_from_https_url() {
        let (owner, repo) = split_repo("https://github.com/Made-by-Moonlight/Athene.git").unwrap();
        assert_eq!(owner, "Made-by-Moonlight");
        assert_eq!(repo, "Athene");
    }

    #[test]
    fn resolve_token_prefers_config_over_env() {
        let token = resolve_token(Some("config-token".to_string()));
        assert_eq!(token, Some("config-token".to_string()));
    }

    fn init_repo(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-C", &dir.to_string_lossy()])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
    }

    #[test]
    fn candidate_repos_returns_empty_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(candidate_repos(&dir.path().to_string_lossy()).is_empty());
    }

    /// The environment this bug was filed for has both a personal `origin`
    /// (HTTPS) remote and an internal mirror reached over SSH through a
    /// custom host alias — `origin` must come first (no extra requests in
    /// the common case), but the mirror must still be found.
    #[test]
    fn candidate_repos_lists_origin_first_then_other_remotes() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let workspace = dir.path().to_string_lossy().to_string();
        std::process::Command::new("git")
            .args(["-C", &workspace, "remote", "add", "internal", "git@github.com-synthesia:Synthesia-Technologies/ninox.git"])
            .status().unwrap();
        std::process::Command::new("git")
            .args(["-C", &workspace, "remote", "add", "origin", "https://github.com/Made-by-Moonlight/ninox.git"])
            .status().unwrap();

        let repos = candidate_repos(&workspace);
        assert_eq!(repos, vec!["Made-by-Moonlight/ninox".to_string(), "Synthesia-Technologies/ninox".to_string()]);
    }

    #[test]
    fn current_branch_reads_checked_out_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let workspace = dir.path().to_string_lossy().to_string();
        std::process::Command::new("git")
            .args(["-C", &workspace, "checkout", "-q", "-b", "feat/my-fix"])
            .status().unwrap();
        assert_eq!(current_branch(&workspace).as_deref(), Some("feat/my-fix"));
    }

    #[test]
    fn current_branch_none_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_branch(&dir.path().to_string_lossy()), None);
    }
}
