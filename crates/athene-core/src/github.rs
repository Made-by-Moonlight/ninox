use anyhow::{Context, Result};
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
struct GhPr {
    number:    u64,
    title:     String,
    state:     String,
    merged:    bool,
    mergeable: Option<bool>,
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
            .user_agent("athene-native/0.1")
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, token })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub async fn get_pr_status(
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
        })
    }

    pub async fn get_ci_checks(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<CheckRun>> {
        // Fetch the commit SHA from the PR first
        let pr_url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}"
        );
        #[derive(Deserialize)]
        struct PrHead { sha: String }
        #[derive(Deserialize)]
        struct PrForSha { head: PrHead }
        let pr: PrForSha = self
            .http
            .get(&pr_url)
            .header(header::AUTHORIZATION, self.auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let sha = pr.head.sha;

        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/commits/{sha}/check-runs?per_page=100"
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

    pub async fn get_review_threads(
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse "owner/repo" or "github.com/owner/repo" into (owner, repo).
pub fn split_repo(s: &str) -> Option<(String, String)> {
    let s = s.trim_start_matches("https://").trim_start_matches("github.com/");
    let mut parts = s.trim_start_matches('/').splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() { return None; }
    Some((owner, repo))
}

/// Resolve GitHub token: prefer explicit config value, fall back to env var.
pub fn resolve_token(config_token: Option<String>) -> Option<String> {
    config_token.or_else(|| std::env::var("GITHUB_TOKEN").ok())
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
    fn resolve_token_prefers_config_over_env() {
        let token = resolve_token(Some("config-token".to_string()));
        assert_eq!(token, Some("config-token".to_string()));
    }
}
