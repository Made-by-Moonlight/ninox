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
    /// Fields only the GraphQL path fetches; `Default` on the REST path.
    pub meta:           PrMeta,
}

/// PR facts beyond the REST-shaped `PrStatus`, consumed by
/// `crate::pr_events` and `ninox pr status`. `None`/empty means "not
/// fetched", never "false".
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PrMeta {
    pub url:                Option<String>,
    pub is_draft:           Option<bool>,
    /// GitHub's `mergeStateStatus`: `BEHIND`, `DIRTY`, `BLOCKED`, `CLEAN`, ...
    pub merge_state:        Option<String>,
    /// `None` when GitHub returned no list (field null on a partial error).
    pub review_requests:    Option<Vec<String>>,
    pub unresolved_threads: Vec<ThreadSummary>,
    /// The token's own login, so deliveries can flag comments this account
    /// wrote (a worker's own replies come back through the same account).
    pub viewer_login:       Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadSummary {
    pub author:  String,
    pub path:    Option<String>,
    pub line:    Option<u32>,
    pub excerpt: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimitInfo {
    pub cost:      u64,
    pub remaining: u64,
    pub limit:     u64,
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

    /// Longest age of a PR's cached detail before a full re-read, even when
    /// its light status is unchanged. No-op for implementations without a
    /// detail cache.
    fn set_detail_refresh_secs(&self, _secs: u64) {}
}

/// Max PR keys per aliased query; branch keys ride in the first chunk with
/// room. Keeps individual GraphQL query complexity/cost bounded.
pub const CHUNK_SIZE: usize = 50;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// One POST of a GraphQL document; the seam tests replace.
#[async_trait]
pub trait GraphQlTransport: Send + Sync {
    async fn post(&self, query: &str) -> Result<Value>;
}

struct HttpTransport {
    http:  Client,
    token: String,
}

#[async_trait]
impl GraphQlTransport for HttpTransport {
    async fn post(&self, query: &str) -> Result<Value> {
        let resp = self
            .http
            .post("https://api.github.com/graphql")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            // `mergeStateStatus` was introduced behind this preview; sending
            // it is harmless where the field has graduated.
            .header(header::ACCEPT, "application/vnd.github.merge-info-preview+json")
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

/// What the cheap phase-1 query reads per PR. Any difference from the last
/// detail read means the PR moved and phase 2 must re-read it. `updatedAt`
/// covers commits, comments and reviews; `checks` (rollup state plus
/// per-state counts) covers a check or status finishing with no PR activity.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct LightPr {
    pub state:           String,
    pub merged:          bool,
    pub mergeable:       Option<String>,
    pub merge_state:     Option<String>,
    pub is_draft:        Option<bool>,
    pub head_sha:        String,
    pub updated_at:      String,
    pub review_decision: Option<String>,
    pub checks:          String,
}

struct CachedPr {
    light:     LightPr,
    snapshot:  PrSnapshot,
    detail_at: std::time::Instant,
}

/// Two-phase GitHub reader. Phase 1 asks a small fixed-shape question about
/// every tracked PR; phase 2 (the expensive nested read of checks, reviews
/// and comments) runs only for PRs whose phase-1 answer changed, that have
/// never been read, or whose last detail read is older than
/// `detail_refresh_secs`. Unchanged PRs return their cached snapshot, so
/// consumers see the same data they would have from a full read.
pub struct GraphQlClient {
    transport:           Box<dyn GraphQlTransport>,
    cache:               std::sync::Mutex<HashMap<PrKey, CachedPr>>,
    detail_refresh_secs: std::sync::atomic::AtomicU64,
}

impl GraphQlClient {
    pub fn new(token: String) -> Result<Self> {
        let http = Client::builder()
            .user_agent("ninox/0.1")
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self::with_transport(Box::new(HttpTransport { http, token })))
    }

    pub(crate) fn with_transport(transport: Box<dyn GraphQlTransport>) -> Self {
        Self {
            transport,
            cache: std::sync::Mutex::new(HashMap::new()),
            detail_refresh_secs: std::sync::atomic::AtomicU64::new(
                crate::config::GithubConfig::default().detail_refresh_secs,
            ),
        }
    }

    /// Phase 1: light status for every PR plus branch lookups, chunked.
    async fn fetch_light(
        &self,
        prs: &[PrKey],
        branches: &[BranchKey],
    ) -> Result<(HashMap<PrKey, LightPr>, HashMap<BranchKey, Option<PrRef>>, RateLimitInfo)> {
        let mut lights = HashMap::new();
        let mut branch_prs = HashMap::new();
        let mut rate = RateLimitInfo::default();
        let mut cost = 0u64;
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
            let query = build_light_query(pr_chunk, br_chunk).expect("non-empty chunk always builds a query");
            let body = self.transport.post(&query).await?;
            let (l, b, r) = parse_light_response(pr_chunk, br_chunk, &body);
            lights.extend(l);
            branch_prs.extend(b);
            cost += r.cost;
            rate = r;
            pr_start = pr_end;
            chunk_index += 1;
            if pr_start >= prs.len() {
                break;
            }
        }
        rate.cost = cost;
        Ok((lights, branch_prs, rate))
    }

    /// Phase 2: full detail for `keys`, chunked. Returns the summed cost and
    /// the last chunk's budget reading.
    async fn fetch_detail(
        &self,
        keys: &[PrKey],
    ) -> Result<(HashMap<PrKey, PrSnapshot>, std::collections::HashSet<PrKey>, RateLimitInfo)> {
        let mut prs = HashMap::new();
        let mut degraded = std::collections::HashSet::new();
        let mut rate = RateLimitInfo::default();
        let mut cost = 0u64;
        for chunk in keys.chunks(CHUNK_SIZE) {
            let query = build_query(chunk, &[]).expect("non-empty chunk always builds a query");
            let body = self.transport.post(&query).await?;
            let r = parse_response(chunk, &[], &body);
            // Partial errors null fields yet still parse; such a snapshot
            // is served this tick but never cached as authoritative.
            if body.get("errors").is_some_and(|e| !e.is_null()) {
                degraded.extend(chunk.iter().cloned());
            }
            prs.extend(r.prs);
            cost += r.rate_limit.cost;
            rate = r.rate_limit;
        }
        rate.cost = cost;
        Ok((prs, degraded, rate))
    }
}

#[async_trait]
impl GithubBatchApi for GraphQlClient {
    fn set_detail_refresh_secs(&self, secs: u64) {
        self.detail_refresh_secs.store(secs, std::sync::atomic::Ordering::Relaxed);
    }

    async fn fetch_batch(&self, prs: &[PrKey], branches: &[BranchKey]) -> Result<BatchResult> {
        if prs.is_empty() && branches.is_empty() {
            return Ok(BatchResult::default());
        }

        let (lights, branch_prs, mut rate) = self.fetch_light(prs, branches).await?;

        let max_age = std::time::Duration::from_secs(
            self.detail_refresh_secs.load(std::sync::atomic::Ordering::Relaxed),
        );
        let now = std::time::Instant::now();
        let need_detail: Vec<PrKey> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            prs.iter()
                .filter(|k| match (lights.get(*k), cache.get(*k)) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(l), Some(c)) => c.light != *l || now.duration_since(c.detail_at) >= max_age,
                })
                .cloned()
                .collect()
        };

        let mut detailed = HashMap::new();
        let mut degraded = std::collections::HashSet::new();
        if !need_detail.is_empty() {
            let (d, bad, r) = self.fetch_detail(&need_detail).await?;
            detailed = d;
            degraded = bad;
            rate = RateLimitInfo { cost: rate.cost + r.cost, ..r };
        }

        let mut out = BatchResult { branch_prs, rate_limit: rate, ..Default::default() };
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for key in prs {
            let Some(light) = lights.get(key) else {
                cache.remove(key);
                continue;
            };
            if let Some(snap) = detailed.remove(key) {
                if degraded.contains(key) {
                    cache.remove(key);
                } else {
                    cache.insert(key.clone(), CachedPr { light: light.clone(), snapshot: snap.clone(), detail_at: now });
                }
                out.prs.insert(key.clone(), snap);
            } else if need_detail.contains(key) {
                // Detail alias errored though the light one resolved:
                // report it missing, and re-read next tick.
                cache.remove(key);
            } else if let Some(c) = cache.get(key) {
                out.prs.insert(key.clone(), c.snapshot.clone());
            }
        }
        let wanted: std::collections::HashSet<&PrKey> = prs.iter().collect();
        cache.retain(|k, _| wanted.contains(k));
        Ok(out)
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

    let mut q = String::from("query {\n  rateLimit { cost remaining limit resetAt }\n  viewer { login }\n");

    for (i, pr) in prs.iter().enumerate() {
        let Some((owner, name)) = split_repo(&pr.repo) else { continue };
        let owner_q = serde_json::to_string(&owner).unwrap_or_else(|_| "\"\"".to_string());
        let name_q = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string());
        let number = pr.number;
        q.push_str(&format!(
            "  pr{i}: repository(owner: {owner_q}, name: {name_q}) {{\n    pullRequest(number: {number}) {{\n"
        ));
        q.push_str("      number title url state merged mergeable mergeStateStatus isDraft headRefOid headRefName baseRefName\n");
        q.push_str("      reviewRequests(first: 10) { nodes { requestedReviewer {\n");
        // No `... on Team` fragment: team fields need read:org, which a
        // plain repo token lacks. A team request reads as its typename.
        q.push_str("        __typename ... on User { login } ... on Bot { login } ... on Mannequin { login }\n");
        q.push_str("      } } }\n");
        q.push_str("      commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100) { nodes {\n");
        q.push_str("        __typename\n");
        q.push_str("        ... on CheckRun { name status conclusion }\n");
        q.push_str("        ... on StatusContext { context state }\n");
        q.push_str("      } } } } } }\n");
        q.push_str("      reviews(last: 20) { nodes { databaseId author { login } body state submittedAt } }\n");
        q.push_str("      reviewThreads(last: 30) { nodes { isResolved comments(last: 10) { nodes {\n");
        q.push_str("        databaseId author { login } body path line createdAt\n");
        q.push_str("      } } } }\n");
        q.push_str("      comments(last: 20) { nodes { databaseId author { login } body createdAt } }\n");
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

/// Phase-1 query: the fields that reveal whether a PR moved, nothing nested
/// beyond a one-node rollup read for its state and per-state counts.
pub(crate) fn build_light_query(prs: &[PrKey], branches: &[BranchKey]) -> Option<String> {
    if prs.is_empty() && branches.is_empty() {
        return None;
    }
    let mut q = String::from("query {\n  rateLimit { cost remaining limit resetAt }\n");
    for (i, pr) in prs.iter().enumerate() {
        let Some((owner, name)) = split_repo(&pr.repo) else { continue };
        let owner_q = serde_json::to_string(&owner).unwrap_or_else(|_| "\"\"".to_string());
        let name_q = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string());
        let number = pr.number;
        q.push_str(&format!(
            "  pr{i}: repository(owner: {owner_q}, name: {name_q}) {{\n    pullRequest(number: {number}) {{\n"
        ));
        q.push_str("      number state merged mergeable mergeStateStatus isDraft headRefOid updatedAt reviewDecision\n");
        q.push_str("      commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 1) {\n");
        q.push_str("        totalCount checkRunCountsByState { state count } statusContextCountsByState { state count }\n");
        q.push_str("      } } } } }\n");
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

fn count_pairs(v: Option<&Value>) -> String {
    let mut pairs: Vec<String> = v
        .and_then(|a| a.as_array())
        .map(|a| a.iter().map(|e| format!("{}={}", str_field(e, "state"), u64_field(e, "count"))).collect())
        .unwrap_or_default();
    pairs.sort();
    pairs.join(",")
}

pub(crate) fn parse_light_response(
    prs: &[PrKey],
    branches: &[BranchKey],
    body: &Value,
) -> (HashMap<PrKey, LightPr>, HashMap<BranchKey, Option<PrRef>>, RateLimitInfo) {
    let mut lights = HashMap::new();
    let mut branch_prs = HashMap::new();
    let mut rate = RateLimitInfo::default();
    let Some(data) = body.get("data").filter(|d| !d.is_null()) else {
        return (lights, branch_prs, rate);
    };
    if let Some(rl) = data.get("rateLimit") {
        rate = RateLimitInfo {
            cost:      u64_field(rl, "cost"),
            remaining: u64_field(rl, "remaining"),
            limit:     u64_field(rl, "limit"),
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
        let rollup = pr
            .get("commits")
            .and_then(|c| c.get("nodes"))
            .and_then(|n| n.as_array())
            .and_then(|a| a.first())
            .and_then(|n| n.get("commit"))
            .and_then(|c| c.get("statusCheckRollup"))
            .filter(|r| !r.is_null());
        let checks = match rollup {
            Some(r) => {
                let ctx = r.get("contexts");
                format!(
                    "{}|{}|{}|{}",
                    str_field(r, "state"),
                    ctx.map(|c| u64_field(c, "totalCount")).unwrap_or(0),
                    count_pairs(ctx.and_then(|c| c.get("checkRunCountsByState"))),
                    count_pairs(ctx.and_then(|c| c.get("statusContextCountsByState"))),
                )
            }
            None => String::new(),
        };
        lights.insert(
            key.clone(),
            LightPr {
                state:           str_field(pr, "state"),
                merged:          bool_field(pr, "merged"),
                mergeable:       opt_str_field(pr, "mergeable"),
                merge_state:     opt_str_field(pr, "mergeStateStatus"),
                is_draft:        pr.get("isDraft").and_then(|v| v.as_bool()),
                head_sha:        str_field(pr, "headRefOid"),
                updated_at:      str_field(pr, "updatedAt"),
                review_decision: opt_str_field(pr, "reviewDecision"),
                checks,
            },
        );
    }
    for (i, key) in branches.iter().enumerate() {
        let pr_ref = data
            .get(format!("br{i}").as_str())
            .and_then(|r| r.get("pullRequests"))
            .and_then(|p| p.get("nodes"))
            .and_then(|n| n.as_array())
            .and_then(|a| a.first())
            .map(|node| PrRef { number: u64_field(node, "number"), url: str_field(node, "url") });
        branch_prs.insert(key.clone(), pr_ref);
    }
    (lights, branch_prs, rate)
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

/// `reviewRequests.nodes[].requestedReviewer`: a user/bot login, or
/// `"a team"` for a team request. `None` when the list itself is missing.
fn review_request_logins(pr: &Value) -> Option<Vec<String>> {
    let nodes = pr.get("reviewRequests")?.get("nodes")?.as_array()?;
    Some(
        nodes.iter()
            .filter_map(|n| n.get("requestedReviewer"))
            .filter_map(|r| {
                opt_str_field(r, "login")
                    .or_else(|| (str_field(r, "__typename") == "Team").then(|| "a team".to_string()))
            })
            .collect(),
    )
}

/// One summary per unresolved review thread, from its first comment.
fn unresolved_threads(pr: &Value) -> Vec<ThreadSummary> {
    get_nodes(pr, "reviewThreads")
        .iter()
        .filter(|t| !bool_field(t, "isResolved"))
        .filter_map(|t| get_nodes(t, "comments").first())
        .map(|c| ThreadSummary {
            author:  author_login(c),
            path:    opt_str_field(c, "path"),
            line:    c.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
            excerpt: crate::pr_events::excerpt(&str_field(c, "body")),
        })
        .collect()
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
            limit:     u64_field(rl, "limit"),
            reset_at:  opt_str_field(rl, "resetAt").as_deref().map(parse_github_timestamp).unwrap_or(0),
        };
    }

    let viewer_login = data.get("viewer").and_then(|v| opt_str_field(v, "login"));

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
        let head_ref = str_field(pr, "headRefName");
        let base_ref = str_field(pr, "baseRefName");
        let closed = gh_state == "CLOSED" && !merged;
        let state = rest_state(&gh_state);

        let status = PrStatus { merged, state, mergeable, title, number, head_sha, head_ref, base_ref };

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

        let meta = PrMeta {
            url:                opt_str_field(pr, "url").filter(|u| !u.is_empty()),
            is_draft:           pr.get("isDraft").and_then(|v| v.as_bool()),
            merge_state:        opt_str_field(pr, "mergeStateStatus"),
            review_requests:    review_request_logins(pr),
            unresolved_threads: unresolved_threads(pr),
            viewer_login:       viewer_login.clone(),
        };

        out.prs.insert(key.clone(), PrSnapshot { status, closed, checks, threads, issue_comments, meta });
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
        assert!(q.contains("rateLimit { cost remaining limit resetAt }"));
        assert!(q.contains(r#"pr0: repository(owner: "o", name: "r")"#));
        assert!(q.contains("pullRequest(number: 7)"));
        assert!(q.contains("headRefName baseRefName"), "must fetch branch refs for stacked-dep derivation");
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
                    "headRefName": "feat/x", "baseRefName": "feat/base",
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
        assert_eq!(snap.status.head_ref, "feat/x");
        assert_eq!(snap.status.base_ref, "feat/base");
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
    fn parse_response_maps_pr_meta() {
        let body: serde_json::Value = serde_json::json!({
            "data": {
                "viewer": { "login": "slievr" },
                "pr0": { "pullRequest": {
                    "number": 7, "title": "T", "url": "https://github.com/o/r/pull/7", "state": "OPEN",
                    "merged": false, "mergeable": "MERGEABLE", "mergeStateStatus": "BEHIND", "isDraft": true,
                    "headRefOid": "abc",
                    "reviewRequests": { "nodes": [
                        { "requestedReviewer": { "__typename": "User", "login": "alice" } },
                        { "requestedReviewer": { "__typename": "Team" } }
                    ] },
                    "reviewThreads": { "nodes": [
                        { "isResolved": false, "comments": { "nodes": [ { "databaseId": 1,
                            "author": { "login": "cursor" }, "body": "possible bug", "path": "src/a.rs", "line": 4 } ] } },
                        { "isResolved": true, "comments": { "nodes": [ { "databaseId": 2,
                            "author": { "login": "bob" }, "body": "done", "path": "src/b.rs", "line": 1 } ] } }
                    ] }
                } }
            }
        });
        let prs = vec![PrKey { repo: "o/r".into(), number: 7 }];
        let meta = &parse_response(&prs, &[], &body).prs[&prs[0]].meta;
        assert_eq!(meta.url.as_deref(), Some("https://github.com/o/r/pull/7"));
        assert_eq!(meta.is_draft, Some(true));
        assert_eq!(meta.merge_state.as_deref(), Some("BEHIND"));
        assert_eq!(meta.review_requests, Some(vec!["alice".to_string(), "a team".to_string()]));
        assert_eq!(meta.viewer_login.as_deref(), Some("slievr"));
        assert_eq!(meta.unresolved_threads.len(), 1, "resolved threads are left out");
        assert_eq!(meta.unresolved_threads[0].author, "cursor");
        assert_eq!(meta.unresolved_threads[0].path.as_deref(), Some("src/a.rs"));
        assert_eq!(meta.unresolved_threads[0].line, Some(4));
    }

    #[test]
    fn build_query_requests_the_event_fields() {
        let q = build_query(&[PrKey { repo: "o/r".into(), number: 1 }], &[]).unwrap();
        for field in ["viewer { login }", "mergeStateStatus", "isDraft", "reviewRequests", "isResolved", " url "] {
            assert!(q.contains(field), "query must request {field}: {q}");
        }
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

    // ── Two-phase client ─────────────────────────────────────────────────

    use std::sync::{Arc, Mutex};

    /// What the fake GitHub knows about one PR.
    #[derive(Clone)]
    struct FakePr {
        updated_at: String,
        passing:    u64,
        pending:    u64,
    }

    struct FakeGithub {
        prs:     Mutex<HashMap<u64, FakePr>>,
        queries: Mutex<Vec<String>>,
        remaining: Mutex<u64>,
        with_errors: Mutex<bool>,
    }

    impl FakeGithub {
        fn new(numbers: &[u64]) -> Arc<Self> {
            let prs = numbers
                .iter()
                .map(|n| (*n, FakePr { updated_at: "t0".into(), passing: 1, pending: 0 }))
                .collect();
            Arc::new(Self { prs: Mutex::new(prs), queries: Mutex::new(vec![]), remaining: Mutex::new(5000), with_errors: Mutex::new(false) })
        }

        fn is_detail(q: &str) -> bool {
            q.contains("reviewThreads")
        }

        fn numbers_in(q: &str) -> Vec<u64> {
            q.lines()
                .filter_map(|l| l.trim().strip_prefix("pullRequest(number: "))
                .filter_map(|l| l.split(')').next()?.parse().ok())
                .collect()
        }

        fn detail_queries(&self) -> Vec<Vec<u64>> {
            self.queries.lock().unwrap().iter().filter(|q| Self::is_detail(q)).map(|q| Self::numbers_in(q)).collect()
        }

        fn light_queries(&self) -> usize {
            self.queries.lock().unwrap().iter().filter(|q| !Self::is_detail(q)).count()
        }
    }

    struct FakeTransport(Arc<FakeGithub>);

    #[async_trait]
    impl GraphQlTransport for FakeTransport {
        async fn post(&self, query: &str) -> Result<Value> {
            self.0.queries.lock().unwrap().push(query.to_string());
            let detail = FakeGithub::is_detail(query);
            let mut data = serde_json::Map::new();
            let cost = if detail { 40 } else { 2 };
            let remaining = {
                let mut r = self.0.remaining.lock().unwrap();
                *r -= cost;
                *r
            };
            data.insert("rateLimit".into(), serde_json::json!({
                "cost": cost, "remaining": remaining, "limit": 5000, "resetAt": "2030-01-01T00:00:00Z",
            }));
            let prs = self.0.prs.lock().unwrap();
            for (i, n) in FakeGithub::numbers_in(query).into_iter().enumerate() {
                let Some(p) = prs.get(&n) else { continue };
                let mut pr = serde_json::json!({
                    "number": n, "title": format!("pr {n}"), "state": "OPEN", "merged": false,
                    "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "isDraft": false,
                    "headRefOid": "abc", "headRefName": "h", "baseRefName": "b",
                    "updatedAt": p.updated_at, "reviewDecision": null,
                });
                let counts = serde_json::json!([
                    {"state": "SUCCESS", "count": p.passing}, {"state": "IN_PROGRESS", "count": p.pending},
                ]);
                pr["commits"] = serde_json::json!({"nodes": [{"commit": {"statusCheckRollup": {
                    "state": if p.pending > 0 { "PENDING" } else { "SUCCESS" },
                    "contexts": {"totalCount": p.passing + p.pending,
                                 "checkRunCountsByState": counts, "statusContextCountsByState": []},
                }}}]});
                data.insert(format!("pr{i}"), serde_json::json!({ "pullRequest": pr }));
            }
            if detail && *self.0.with_errors.lock().unwrap() {
                return Ok(serde_json::json!({ "data": data, "errors": [{"message": "partial"}] }));
            }
            Ok(serde_json::json!({ "data": data }))
        }
    }

    fn client(gh: &Arc<FakeGithub>) -> GraphQlClient {
        GraphQlClient::with_transport(Box::new(FakeTransport(gh.clone())))
    }

    fn keys(numbers: &[u64]) -> Vec<PrKey> {
        numbers.iter().map(|n| PrKey { repo: "o/r".into(), number: *n }).collect()
    }

    #[tokio::test]
    async fn only_changed_prs_get_the_detail_query() {
        let gh = FakeGithub::new(&[1, 2, 3]);
        let c = client(&gh);
        let all = keys(&[1, 2, 3]);

        let first = c.fetch_batch(&all, &[]).await.unwrap();
        assert_eq!(first.prs.len(), 3);
        assert_eq!(gh.detail_queries(), vec![vec![1, 2, 3]], "first sight reads every PR in full");

        let second = c.fetch_batch(&all, &[]).await.unwrap();
        assert_eq!(second.prs.len(), 3, "unchanged PRs still come back, from the cache");
        assert_eq!(gh.detail_queries().len(), 1, "nothing changed: no new detail query");
        assert_eq!(gh.light_queries(), 2);

        gh.prs.lock().unwrap().get_mut(&2).unwrap().updated_at = "t1".into();
        c.fetch_batch(&all, &[]).await.unwrap();
        assert_eq!(gh.detail_queries().last().unwrap(), &vec![2], "only the PR whose updatedAt moved");
    }

    #[tokio::test]
    async fn a_check_finishing_with_no_pr_activity_still_triggers_a_detail_read() {
        let gh = FakeGithub::new(&[1]);
        gh.prs.lock().unwrap().get_mut(&1).unwrap().pending = 2;
        let c = client(&gh);
        c.fetch_batch(&keys(&[1]), &[]).await.unwrap();

        // One of two pending checks finishes: rollup stays PENDING and
        // updatedAt is unchanged, but the per-state counts move.
        {
            let mut prs = gh.prs.lock().unwrap();
            let p = prs.get_mut(&1).unwrap();
            p.pending = 1;
            p.passing = 2;
        }
        c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(gh.detail_queries().len(), 2);
    }

    #[tokio::test]
    async fn detail_is_re_read_once_older_than_the_refresh_window() {
        let gh = FakeGithub::new(&[1]);
        let c = client(&gh);
        c.set_detail_refresh_secs(0);
        c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(gh.detail_queries().len(), 2, "a zero max age re-reads every tick");
    }

    #[tokio::test]
    async fn a_detail_response_with_errors_is_served_but_not_cached() {
        let gh = FakeGithub::new(&[1]);
        *gh.with_errors.lock().unwrap() = true;
        let c = client(&gh);
        let first = c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(first.prs.len(), 1, "the degraded snapshot is still served");
        *gh.with_errors.lock().unwrap() = false;
        c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(gh.detail_queries().len(), 2, "next tick re-reads instead of trusting it");
        c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(gh.detail_queries().len(), 2, "a clean read is cached");
    }

    #[tokio::test]
    async fn dropped_and_missing_prs_leave_the_cache() {
        let gh = FakeGithub::new(&[1, 2]);
        let c = client(&gh);
        c.fetch_batch(&keys(&[1, 2]), &[]).await.unwrap();
        let only_one = c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(only_one.prs.len(), 1);
        c.fetch_batch(&keys(&[1, 2]), &[]).await.unwrap();
        assert_eq!(gh.detail_queries().last().unwrap(), &vec![2], "PR 2 was evicted, so it is read in full again");

        let missing = c.fetch_batch(&keys(&[1, 99]), &[]).await.unwrap();
        assert!(!missing.prs.contains_key(&PrKey { repo: "o/r".into(), number: 99 }));
    }

    #[tokio::test]
    async fn cost_is_summed_across_phases_and_the_last_budget_reading_is_kept() {
        let gh = FakeGithub::new(&[1]);
        let c = client(&gh);
        let first = c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(first.rate_limit.cost, 42, "light 2 + detail 40");
        assert_eq!((first.rate_limit.remaining, first.rate_limit.limit), (4958, 5000));
        let second = c.fetch_batch(&keys(&[1]), &[]).await.unwrap();
        assert_eq!(second.rate_limit.cost, 2, "an idle tick costs only the light query");
    }

    #[test]
    fn light_query_is_flat_and_the_detail_query_is_bounded() {
        let light = build_light_query(&keys(&[1]), &[]).unwrap();
        for nested in ["reviewThreads", "reviews(", "comments(", "reviewRequests", "contexts(first: 100)"] {
            assert!(!light.contains(nested), "light query must not read {nested}");
        }
        assert!(light.contains("updatedAt") && light.contains("headRefOid") && light.contains("reviewDecision"));
        let detail = build_query(&keys(&[1]), &[]).unwrap();
        for bounded in [
            "reviewThreads(last: 30)", "comments(last: 10)", "comments(last: 20)", "reviews(last: 20)",
            "contexts(first: 100)", "reviewRequests(first: 10)",
        ] {
            assert!(detail.contains(bounded), "detail query missing {bounded}");
        }
    }
}
