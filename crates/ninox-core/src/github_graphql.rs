//! Batched GraphQL fetch for PR watching.
//!
//! Fetches many PRs (and open-PR-for-branch lookups) in a single aliased
//! GraphQL query, mapping the results into the same REST-shaped types
//! `github.rs` already produces (`PrStatus`, `CheckRun`, `ReviewThread`,
//! `Comment`) so downstream enrichment code doesn't need to know whether a
//! given field came from the REST or GraphQL path.

use crate::github::{parse_github_timestamp, split_repo, CheckRun, PrRef, PrStatus, ReviewThread};
use crate::types::Comment;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{header, Client};
use serde_json::Value;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// A PR to fetch: repo slug ("owner/name") + number.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub repo:   String,
    pub number: u64,
}

/// A branch to reconcile: repo slug + head branch name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchKey {
    pub repo:   String,
    pub branch: String,
}

/// Everything the poller's enrichment needs for one PR, from one query.
#[derive(Debug, Clone)]
pub struct PrSnapshot {
    pub status:         PrStatus, // merged/state/mergeable/title/number/head_sha
    pub closed:         bool,     // state == CLOSED && !merged → auto-close watches
    pub checks:         Vec<CheckRun>,
    pub threads:        Vec<ReviewThread>, // reviews + inline review comments
    pub issue_comments: Vec<Comment>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimitInfo {
    pub cost:      u64,
    pub remaining: u64,
    pub reset_at:  i64, // unix millis
}

#[derive(Debug, Default)]
pub struct BatchResult {
    /// Per-alias result; a PrKey absent from the map means the alias errored
    /// (deleted repo, bad number) — callers treat that like a lookup failure.
    pub prs:        HashMap<PrKey, PrSnapshot>,
    /// Open PR found per branch key, if any.
    pub branch_prs: HashMap<BranchKey, Option<PrRef>>,
    pub rate_limit: RateLimitInfo,
}

/// Returned (wrapped in `anyhow::Error`) when GitHub's GraphQL endpoint
/// answers 403/429. Callers recover the `Retry-After` hint (if GitHub sent
/// one) via `err.downcast_ref::<RateLimitedError>()`.
#[derive(Debug)]
pub struct RateLimitedError {
    pub retry_after_secs: Option<u64>,
}

impl std::fmt::Display for RateLimitedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after_secs {
            Some(secs) => write!(f, "GitHub GraphQL rate limited; retry after {secs}s"),
            None => write!(f, "GitHub GraphQL rate limited"),
        }
    }
}

impl std::error::Error for RateLimitedError {}

// ---------------------------------------------------------------------------
// Trait — allows the poller to be driven by a fake in tests, without any
// network access.
// ---------------------------------------------------------------------------

#[async_trait]
pub trait GithubBatchApi: Send + Sync {
    /// One aliased GraphQL query (chunked at CHUNK_SIZE) covering all keys.
    async fn fetch_batch(&self, prs: &[PrKey], branches: &[BranchKey]) -> Result<BatchResult>;
}

/// Max PR keys per aliased query; branch keys ride in the first chunk with
/// room. Keeps individual GraphQL query complexity/cost bounded.
pub const CHUNK_SIZE: usize = 50;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct GraphQlClient {
    http:  Client,
    token: String,
}

impl GraphQlClient {
    pub fn new(token: String) -> Result<Self> {
        let http = Client::builder()
            .user_agent("ninox/0.1")
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, token })
    }

    async fn post_query(&self, query: &str) -> Result<Value> {
        let resp = self
            .http
            .post("https://api.github.com/graphql")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            return Err(anyhow::Error::new(RateLimitedError { retry_after_secs }));
        }

        let body: Value = resp.error_for_status()?.json().await?;
        if body.get("data").map(|d| d.is_null()).unwrap_or(true) {
            anyhow::bail!(
                "GitHub GraphQL query returned no data: {:?}",
                body.get("errors")
            );
        }
        Ok(body)
    }
}

#[async_trait]
impl GithubBatchApi for GraphQlClient {
    async fn fetch_batch(&self, prs: &[PrKey], branches: &[BranchKey]) -> Result<BatchResult> {
        if prs.is_empty() && branches.is_empty() {
            return Ok(BatchResult::default());
        }

        let mut merged = BatchResult::default();
        // Branch keys ride along in the first chunk, in whatever room is
        // left after reserving space for that chunk's PR keys.
        let first_pr_capacity = CHUNK_SIZE.saturating_sub(branches.len());
        let mut pr_start = 0usize;
        let mut chunk_index = 0usize;

        loop {
            let capacity = if chunk_index == 0 { first_pr_capacity } else { CHUNK_SIZE };
            let pr_end = (pr_start + capacity).min(prs.len());
            let pr_chunk = &prs[pr_start..pr_end];
            let br_chunk: &[BranchKey] = if chunk_index == 0 { branches } else { &[] };

            if pr_chunk.is_empty() && br_chunk.is_empty() {
                break;
            }

            let query = build_query(pr_chunk, br_chunk).expect("non-empty chunk always builds a query");
            let body = self.post_query(&query).await?;
            let result = parse_response(pr_chunk, br_chunk, &body);
            merged.prs.extend(result.prs);
            merged.branch_prs.extend(result.branch_prs);
            merged.rate_limit = result.rate_limit; // keep the LAST chunk's

            pr_start = pr_end;
            chunk_index += 1;
            if pr_start >= prs.len() {
                break;
            }
        }

        Ok(merged)
    }
}

// ---------------------------------------------------------------------------
// Query building
// ---------------------------------------------------------------------------

/// Build one aliased GraphQL query covering every key. `None` when both
/// lists are empty — nothing to fetch.
pub(crate) fn build_query(prs: &[PrKey], branches: &[BranchKey]) -> Option<String> {
    if prs.is_empty() && branches.is_empty() {
        return None;
    }

    let mut q = String::from("query {\n  rateLimit { cost remaining resetAt }\n");

    for (i, pr) in prs.iter().enumerate() {
        let Some((owner, name)) = split_repo(&pr.repo) else { continue };
        let owner_q = serde_json::to_string(&owner).unwrap_or_else(|_| "\"\"".to_string());
        let name_q = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string());
        let number = pr.number;
        q.push_str(&format!(
            "  pr{i}: repository(owner: {owner_q}, name: {name_q}) {{\n    pullRequest(number: {number}) {{\n"
        ));
        q.push_str("      number title state merged mergeable headRefOid\n");
        q.push_str("      commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100) { nodes {\n");
        q.push_str("        __typename\n");
        q.push_str("        ... on CheckRun { name status conclusion }\n");
        q.push_str("        ... on StatusContext { context state }\n");
        q.push_str("      } } } } } }\n");
        q.push_str("      reviews(last: 50) { nodes { databaseId author { login } body state submittedAt } }\n");
        q.push_str("      reviewThreads(last: 50) { nodes { comments(last: 50) { nodes {\n");
        q.push_str("        databaseId author { login } body path line createdAt\n");
        q.push_str("      } } } }\n");
        q.push_str("      comments(last: 100) { nodes { databaseId author { login } body createdAt } }\n");
        q.push_str("    }\n  }\n");
    }

    for (i, br) in branches.iter().enumerate() {
        let Some((owner, name)) = split_repo(&br.repo) else { continue };
        let owner_q = serde_json::to_string(&owner).unwrap_or_else(|_| "\"\"".to_string());
        let name_q = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string());
        let branch_q = serde_json::to_string(&br.branch).unwrap_or_else(|_| "\"\"".to_string());
        q.push_str(&format!(
            "  br{i}: repository(owner: {owner_q}, name: {name_q}) {{\n    pullRequests(headRefName: {branch_q}, states: OPEN, first: 1) {{ nodes {{ number url }} }}\n  }}\n"
        ));
    }

    q.push_str("}\n");
    Some(q)
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn opt_str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn i64_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn author_login(v: &Value) -> String {
    v.get("author")
        .and_then(|a| a.get("login"))
        .and_then(|l| l.as_str())
        .unwrap_or("ghost")
        .to_string()
}

/// GraphQL PR `state` → REST-shaped `PrStatus.state` ("open" | "closed").
fn rest_state(gh_state: &str) -> String {
    match gh_state {
        "OPEN" => "open".to_string(),
        "CLOSED" | "MERGED" => "closed".to_string(),
        other => other.to_lowercase(),
    }
}

/// GraphQL `mergeable` enum → REST-shaped `Option<bool>`. `UNKNOWN` (lazy
/// computation still pending on GitHub's side) maps to `None`, same as an
/// absent value — the poller keeps last-known via `derive_gate_status`'s
/// `Unknown` handling.
fn rest_mergeable(gh_mergeable: &str) -> Option<bool> {
    match gh_mergeable {
        "MERGEABLE" => Some(true),
        "CONFLICTING" => Some(false),
        _ => None,
    }
}

/// `contexts.nodes` items are one of two GraphQL interface implementations —
/// `CheckRun` (GitHub Actions/App checks) or `StatusContext` (legacy commit
/// statuses, a bonus over the REST `check-runs` endpoint).
fn parse_check_node(node: &Value) -> Option<CheckRun> {
    match node.get("__typename").and_then(|t| t.as_str())? {
        "CheckRun" => Some(CheckRun {
            name:       str_field(node, "name"),
            status:     str_field(node, "status").to_lowercase(),
            conclusion: opt_str_field(node, "conclusion").map(|c| c.to_lowercase()),
        }),
        "StatusContext" => {
            let name = str_field(node, "context");
            let (status, conclusion) = match str_field(node, "state").as_str() {
                "SUCCESS" => ("completed", Some("success".to_string())),
                "FAILURE" | "ERROR" => ("completed", Some("failure".to_string())),
                // PENDING | EXPECTED | anything else — no conclusion yet.
                _ => ("in_progress", None),
            };
            Some(CheckRun { name, status: status.to_string(), conclusion })
        }
        _ => None,
    }
}

/// `pullRequest.<key>.nodes` — a single level of `{ nodes: [...] }`.
fn get_nodes<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(|o| o.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// `pullRequest.commits.nodes[0].commit.statusCheckRollup.contexts.nodes`.
fn check_nodes(pr: &Value) -> &[Value] {
    pr.get("commits")
        .and_then(|c| c.get("nodes"))
        .and_then(|n| n.as_array())
        .and_then(|arr| arr.first())
        .and_then(|commit_node| commit_node.get("commit"))
        .and_then(|c| c.get("statusCheckRollup"))
        .and_then(|r| r.get("contexts"))
        .and_then(|c| c.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// `pullRequest.reviewThreads.nodes[].comments.nodes[]`, flattened.
fn review_thread_comment_nodes(pr: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if let Some(threads) = pr.get("reviewThreads").and_then(|t| t.get("nodes")).and_then(|n| n.as_array()) {
        for thread in threads {
            if let Some(comments) = thread.get("comments").and_then(|c| c.get("nodes")).and_then(|n| n.as_array()) {
                out.extend(comments.iter());
            }
        }
    }
    out
}

/// Walk a raw GraphQL response body into the REST-shaped `BatchResult`.
/// `prs`/`branches` are the same key slices given to `build_query`, so
/// aliases (`pr{i}`, `br{i}`) can be positionally matched back to keys.
///
/// A 200 response with `data: null` entirely is treated by `fetch_batch`
/// (via `post_query`) as an error before this function is ever called. A
/// per-alias `null` (partial `errors`) is normal here: that key is simply
/// omitted from the result.
pub(crate) fn parse_response(prs: &[PrKey], branches: &[BranchKey], body: &Value) -> BatchResult {
    let mut out = BatchResult::default();

    let Some(data) = body.get("data").filter(|d| !d.is_null()) else {
        return out;
    };

    if let Some(rl) = data.get("rateLimit") {
        out.rate_limit = RateLimitInfo {
            cost:      u64_field(rl, "cost"),
            remaining: u64_field(rl, "remaining"),
            reset_at:  opt_str_field(rl, "resetAt").as_deref().map(parse_github_timestamp).unwrap_or(0),
        };
    }

    for (i, key) in prs.iter().enumerate() {
        let Some(pr) = data
            .get(format!("pr{i}").as_str())
            .and_then(|r| r.get("pullRequest"))
            .filter(|p| !p.is_null())
        else {
            continue;
        };

        let number = pr.get("number").and_then(|v| v.as_u64()).unwrap_or(key.number);
        let title = str_field(pr, "title");
        let gh_state = str_field(pr, "state");
        let merged = bool_field(pr, "merged");
        let mergeable = opt_str_field(pr, "mergeable").as_deref().and_then(rest_mergeable);
        let head_sha = str_field(pr, "headRefOid");
        let closed = gh_state == "CLOSED" && !merged;
        let state = rest_state(&gh_state);

        let status = PrStatus { merged, state, mergeable, title, number, head_sha };

        let checks: Vec<CheckRun> = check_nodes(pr).iter().filter_map(parse_check_node).collect();

        let mut threads: Vec<ReviewThread> = get_nodes(pr, "reviews")
            .iter()
            .map(|r| ReviewThread {
                id:         i64_field(r, "databaseId"),
                author:     author_login(r),
                body:       str_field(r, "body"),
                path:       None,
                line:       None,
                state:      str_field(r, "state"),
                created_at: opt_str_field(r, "submittedAt").as_deref().map(parse_github_timestamp).unwrap_or(0),
            })
            .collect();

        for c in review_thread_comment_nodes(pr) {
            threads.push(ReviewThread {
                id:         i64_field(c, "databaseId"),
                author:     author_login(c),
                body:       str_field(c, "body"),
                path:       opt_str_field(c, "path"),
                line:       c.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
                state:      "COMMENTED".to_string(),
                created_at: opt_str_field(c, "createdAt").as_deref().map(parse_github_timestamp).unwrap_or(0),
            });
        }

        let issue_comments: Vec<Comment> = get_nodes(pr, "comments")
            .iter()
            .map(|c| Comment {
                id:         i64_field(c, "databaseId"),
                pr_id:      number as i64,
                author:     author_login(c),
                body:       str_field(c, "body"),
                path:       None,
                line:       None,
                created_at: opt_str_field(c, "createdAt").as_deref().map(parse_github_timestamp).unwrap_or(0),
            })
            .collect();

        out.prs.insert(key.clone(), PrSnapshot { status, closed, checks, threads, issue_comments });
    }

    for (i, key) in branches.iter().enumerate() {
        let pr_ref = data
            .get(format!("br{i}").as_str())
            .and_then(|r| r.get("pullRequests"))
            .and_then(|p| p.get("nodes"))
            .and_then(|n| n.as_array())
            .and_then(|a| a.first())
            .map(|node| PrRef {
                number: u64_field(node, "number"),
                url:    str_field(node, "url"),
            });
        out.branch_prs.insert(key.clone(), pr_ref);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_query_aliases_prs_branches_and_rate_limit() {
        let q = build_query(
            &[PrKey { repo: "o/r".into(), number: 7 }],
            &[BranchKey { repo: "o/r".into(), branch: "feat/x".into() }],
        ).unwrap();
        assert!(q.contains("rateLimit { cost remaining resetAt }"));
        assert!(q.contains(r#"pr0: repository(owner: "o", name: "r")"#));
        assert!(q.contains("pullRequest(number: 7)"));
        assert!(q.contains(r#"br0: repository(owner: "o", name: "r")"#));
        assert!(q.contains(r#"headRefName: "feat/x""#));
    }

    #[test]
    fn build_query_returns_none_when_empty() {
        assert!(build_query(&[], &[]).is_none());
    }

    #[test]
    fn parse_response_maps_rest_compatible_shapes() {
        let body: serde_json::Value = serde_json::json!({
            "data": {
                "rateLimit": { "cost": 1, "remaining": 4999, "resetAt": "2024-01-02T03:04:05Z" },
                "pr0": { "pullRequest": {
                    "number": 7, "title": "T", "state": "OPEN", "merged": false,
                    "mergeable": "UNKNOWN", "headRefOid": "abc123",
                    "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                        { "__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE" },
                        { "__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS" }
                    ] } } } } ] },
                    "reviews": { "nodes": [ { "databaseId": 11, "author": { "login": "alice" },
                        "body": "please fix", "state": "CHANGES_REQUESTED", "submittedAt": "2024-01-02T03:04:05Z" } ] },
                    "reviewThreads": { "nodes": [ { "comments": { "nodes": [ { "databaseId": 12,
                        "author": { "login": "alice" }, "body": "here", "path": "src/a.rs",
                        "line": 3, "createdAt": "2024-01-02T03:04:05Z" } ] } } ] },
                    "comments": { "nodes": [ { "databaseId": 13, "author": { "login": "bob" },
                        "body": "ping", "createdAt": "2024-01-02T03:04:05Z" } ] }
                } },
                "br0": { "pullRequests": { "nodes": [ { "number": 9, "url": "https://github.com/o/r/pull/9" } ] } }
            }
        });
        let prs = vec![PrKey { repo: "o/r".into(), number: 7 }];
        let brs = vec![BranchKey { repo: "o/r".into(), branch: "feat/x".into() }];
        let out = parse_response(&prs, &brs, &body);

        let snap = out.prs.get(&prs[0]).unwrap();
        assert_eq!(snap.status.state, "open");
        assert!(!snap.status.merged);
        assert_eq!(snap.status.mergeable, None); // UNKNOWN
        assert_eq!(snap.status.head_sha, "abc123");
        assert!(!snap.closed);
        assert_eq!(snap.checks.len(), 2);
        assert_eq!(snap.checks[0].conclusion.as_deref(), Some("failure"));
        assert_eq!(snap.checks[1].name, "ci/legacy");
        assert_eq!(snap.checks[1].conclusion.as_deref(), Some("success"));
        assert_eq!(snap.threads.len(), 2); // review + inline comment
        assert_eq!(snap.threads[0].id, 11);
        assert_eq!(snap.threads[1].state, "COMMENTED");
        assert_eq!(snap.issue_comments[0].id, 13);

        assert_eq!(out.branch_prs.get(&brs[0]).unwrap().as_ref().unwrap().number, 9);
        assert_eq!(out.rate_limit.remaining, 4999);
        assert_eq!(out.rate_limit.reset_at, 1_704_164_645_000);
    }

    #[test]
    fn parse_response_merged_and_closed_states() {
        let mk = |state: &str, merged: bool| serde_json::json!({
            "data": { "pr0": { "pullRequest": {
                "number": 7, "title": "T", "state": state, "merged": merged,
                "mergeable": "MERGEABLE", "headRefOid": "abc",
                "commits": { "nodes": [] }, "reviews": { "nodes": [] },
                "reviewThreads": { "nodes": [] }, "comments": { "nodes": [] }
            } } }
        });
        let prs = vec![PrKey { repo: "o/r".into(), number: 7 }];
        let merged = parse_response(&prs, &[], &mk("MERGED", true));
        assert!(merged.prs[&prs[0]].status.merged);
        assert_eq!(merged.prs[&prs[0]].status.state, "closed");
        assert!(!merged.prs[&prs[0]].closed); // merged, not "closed without merge"
        let closed = parse_response(&prs, &[], &mk("CLOSED", false));
        assert!(closed.prs[&prs[0]].closed);
        assert_eq!(closed.prs[&prs[0]].status.mergeable, Some(true));
    }

    #[test]
    fn parse_response_omits_errored_aliases() {
        let body = serde_json::json!({
            "data": { "pr0": null },
            "errors": [ { "message": "Could not resolve to a Repository" } ]
        });
        let prs = vec![PrKey { repo: "gone/repo".into(), number: 1 }];
        let out = parse_response(&prs, &[], &body);
        assert!(out.prs.is_empty());
    }
}
