use crate::embeddings::{cosine_similarity, reciprocal_rank_fusion, Embedder, QUERY_INSTRUCTION_PREFIX};
use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{params, functions::FunctionFlags, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainEntry {
    pub id: String,         // relative path from brain root (e.g. "people/alice.md")
    pub entry_type: String, // derived from parent directory name
    pub name: String,       // from frontmatter or filename stem
    pub tags: Vec<String>,
    pub repos: Vec<String>,
    pub updated: Option<String>,
    pub body: String,
    /// The catalogue name this entry came from. Empty for a `BrainIndex`
    /// queried directly (it only ever holds one catalogue's worth of
    /// entries, so callers already know the source from context); stamped
    /// with the owning `CatalogueRef.name` by `MergedBrainIndex`, which is
    /// the only thing that mixes entries from more than one catalogue.
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Default)]
pub struct QueryFilters {
    pub entry_type: Option<String>,
    pub tag: Option<String>,
}

/// Canonical, lowercase kind id for grouping/filtering `BrainEntry::entry_type`
/// values. `entry_type` comes from either frontmatter `type:` (often
/// singular, e.g. `decision`) or the containing directory name (always
/// plural, e.g. `decisions/`) — without normalizing, any consumer grouping
/// by the raw string shows both spellings as separate groups. The single
/// shared source of truth for every consumer (GUI, TUI, CLI); unrecognized
/// kinds pass through unchanged (lowercased) rather than being guessed at.
pub fn normalize_kind(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    let canonical = match lower.as_str() {
        "decision" | "decisions" => "decisions",
        "error" | "errors" => "errors",
        "architecture" | "architectures" => "architecture",
        "repo" | "repos" | "repository" | "repositories" => "repos",
        "pattern" | "patterns" => "patterns",
        "relationship" | "relationships" => "relationships",
        "concept" | "concepts" => "concepts",
        "symbol" | "symbols" => "symbols",
        "hazard" | "hazards" => "hazards",
        "project" | "projects" => "project",
        "reference" | "references" => "reference",
        "feedback" | "feedbacks" => "feedback",
        _ => return lower,
    };
    canonical.to_string()
}

/// Display label for a `normalize_kind` value, e.g. `"decisions"` ->
/// `"Decisions"`. Unknown keys are title-cased rather than left as-is, so
/// any kind this list doesn't know about still reads as a normal label.
pub fn kind_label(key: &str) -> String {
    match key {
        "decisions" => "Decisions".to_string(),
        "errors" => "Errors".to_string(),
        "architecture" => "Architecture".to_string(),
        "repos" => "Repos".to_string(),
        "patterns" => "Patterns".to_string(),
        "relationships" => "Relationships".to_string(),
        "concepts" => "Concepts".to_string(),
        "symbols" => "Symbols".to_string(),
        "hazards" => "Hazards".to_string(),
        "project" => "Project".to_string(),
        "reference" => "Reference".to_string(),
        "feedback" => "Feedback".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod kind_normalization_tests {
    use super::*;

    #[test]
    fn normalize_kind_merges_singular_and_plural() {
        assert_eq!(normalize_kind("decision"), "decisions");
        assert_eq!(normalize_kind("decisions"), "decisions");
        assert_eq!(normalize_kind("Decision"), "decisions");
        assert_eq!(normalize_kind("error"), "errors");
        assert_eq!(normalize_kind("errors"), "errors");
    }

    #[test]
    fn normalize_kind_passes_through_unknown_kinds() {
        assert_eq!(normalize_kind("note"), "note");
        assert_eq!(normalize_kind("People"), "people");
    }

    #[test]
    fn kind_label_title_cases_unknown_kinds() {
        assert_eq!(kind_label("decisions"), "Decisions");
        assert_eq!(kind_label("people"), "People");
    }
}

/// `line` is an ATX H1 (`# Heading`, not `##`+) — CommonMark requires the
/// space after the `#`, so this also rejects a bare `#`.
pub fn is_h1_line(line: &str) -> bool {
    line.trim_start().starts_with("# ")
}

/// The first ATX H1 heading in `body`, if any, with the `# ` prefix
/// stripped and surrounding whitespace trimmed. `None` for a blank heading
/// (`# ` with nothing after it) as well as no heading at all.
pub fn first_h1(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let trimmed = line.trim_start();
        is_h1_line(trimmed).then(|| trimmed[2..].trim().to_string())
    }).filter(|t| !t.is_empty())
}

/// An entry's readable title for display (row titles, the reading pane's
/// heading, backlink/outlink chips): the body's first H1, else
/// [`BrainEntry::name`] — which this module already resolves from the
/// frontmatter `name:` field, else the filename stem. Callers use this
/// instead of `entry.name` directly so a body that opens with its own H1
/// (the common case) is what's actually shown, rather than a slug-like
/// frontmatter name or the raw filename stem; the raw stem still belongs in
/// a breadcrumb, which deliberately names the file, not the content. The
/// single shared source of truth for every consumer (GUI, TUI, CLI), same
/// reasoning as `normalize_kind`.
pub fn display_title(entry: &BrainEntry) -> String {
    first_h1(&entry.body).unwrap_or_else(|| entry.name.clone())
}

/// Removes the first ATX H1 line `display_title` would have used as the
/// title (plus one immediately following blank line, so no double gap is
/// left behind) — so a rendered body doesn't repeat the title a second time
/// as its own first heading.
pub fn strip_leading_h1(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut removed = false;
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        if !removed && is_h1_line(line) {
            removed = true;
            if lines.peek().is_some_and(|next| next.trim().is_empty()) {
                lines.next();
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod title_tests {
    use super::*;

    fn entry(name: &str, body: &str) -> BrainEntry {
        BrainEntry {
            id: "decisions/x.md".into(),
            entry_type: "decisions".into(),
            name: name.into(),
            tags: vec![],
            repos: vec![],
            updated: None,
            body: body.into(),
            source: String::new(),
        }
    }

    #[test]
    fn display_title_prefers_the_bodys_first_h1_over_the_frontmatter_name() {
        let e = entry("2026-q3-mlops-okrs", "# 2026-Q3 MLOps OKRs — source of truth\n\nBody text.");
        assert_eq!(display_title(&e), "2026-Q3 MLOps OKRs — source of truth");
    }

    #[test]
    fn display_title_falls_back_to_name_without_an_h1() {
        let e = entry("kata-containers-gpu-slot-viability-2026-10-28", "No heading here, just prose.");
        assert_eq!(display_title(&e), "kata-containers-gpu-slot-viability-2026-10-28");
    }

    #[test]
    fn display_title_ignores_h2_and_deeper() {
        let e = entry("fallback-name", "## Not a title\nBody.");
        assert_eq!(display_title(&e), "fallback-name");
    }

    #[test]
    fn display_title_falls_back_on_a_blank_h1() {
        let e = entry("fallback-name", "# \nBody.");
        assert_eq!(display_title(&e), "fallback-name");
    }

    #[test]
    fn display_title_finds_an_h1_that_is_not_the_first_line() {
        let e = entry("fallback-name", "Some preamble.\n# The real title\nBody.");
        assert_eq!(display_title(&e), "The real title");
    }

    #[test]
    fn strip_leading_h1_removes_the_heading_and_one_following_blank_line() {
        assert_eq!(strip_leading_h1("# Title\n\nBody text.\nMore body."), "Body text.\nMore body.\n");
    }

    #[test]
    fn strip_leading_h1_without_a_trailing_blank_line_keeps_the_rest_intact() {
        assert_eq!(strip_leading_h1("# Title\nBody text."), "Body text.\n");
    }

    #[test]
    fn strip_leading_h1_leaves_body_without_an_h1_untouched() {
        assert_eq!(strip_leading_h1("Just prose.\nMore prose."), "Just prose.\nMore prose.\n");
    }

    #[test]
    fn strip_leading_h1_only_removes_the_first_occurrence() {
        // A later `# ` line (e.g. inside a fenced code block, or a second
        // top-level heading) is left alone — only the title heading goes.
        assert_eq!(strip_leading_h1("# Title\n\nBody.\n# Not removed"), "Body.\n# Not removed\n");
    }
}

// ---------------------------------------------------------------------------
// List filtering and sorting — shared by the GUI's Brain List view and the
// TUI's Brain tab so both apps filter/sort entries the same way.
// ---------------------------------------------------------------------------

/// Entry-list sort order (`Brain.dc.html`'s Recent/A-Z/Most-linked segmented
/// control).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrainSort {
    #[default]
    Recent,
    Az,
    MostLinked,
}

/// (kind key, display label, count), sorted by count descending, ties
/// broken alphabetically by label for a deterministic order.
pub fn kinds_sorted_by_count(entries: &[BrainEntry]) -> Vec<(String, String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in entries {
        *counts.entry(normalize_kind(&e.entry_type)).or_default() += 1;
    }
    let mut out: Vec<(String, String, usize)> =
        counts.into_iter().map(|(k, n)| { let label = kind_label(&k); (k, label, n) }).collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    out
}

/// Entries matching `kind` only (ignores repo/tag/search) — the Kinds
/// column's per-row count and the entry list header's "N" total.
pub fn count_for_kind(entries: &[BrainEntry], kind: Option<&str>) -> usize {
    match kind {
        Some(k) => entries.iter().filter(|e| normalize_kind(&e.entry_type) == k).count(),
        None => entries.len(),
    }
}

/// (value, count), most frequent first then alphabetical, truncated to
/// `limit` — the Kinds column's Repo/Tag chip rows. `extract` pulls the
/// repeated string field (`repos` or `tags`) off each entry.
pub fn top_values<'a>(
    entries: impl Iterator<Item = &'a BrainEntry>,
    extract: impl Fn(&'a BrainEntry) -> &'a [String],
    limit: usize,
) -> (Vec<(String, usize)>, usize) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in entries {
        for v in extract(e) {
            *counts.entry(v.as_str()).or_default() += 1;
        }
    }
    let mut all: Vec<(String, usize)> = counts.into_iter().map(|(v, n)| (v.to_string(), n)).collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = all.len();
    all.truncate(limit);
    (all, total)
}

fn matches_search(e: &BrainEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    e.name.to_lowercase().contains(&q)
        || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
        || e.body.to_lowercase().contains(&q)
}

/// Undirected dedup key for the edge between node indices `a` and `b`:
/// order-independent, so a mutual pair of wikilinks (A -> B and B -> A)
/// collapses to a single key instead of being stroked/counted twice.
fn edge_key(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Resolve `BrainIndex::links_all()`'s `(from_id, to_id)` string pairs into
/// deduplicated, undirected node-index pairs against `entries`'s current
/// order. A link endpoint that isn't in `entries` (index/entries drift, or a
/// link to an id that no longer exists) is skipped rather than panicking; a
/// mutual pair (A -> B and B -> A) collapses to one edge.
pub fn resolve_edges(entries: &[BrainEntry], links: &[(String, String)]) -> Vec<(usize, usize)> {
    let index: HashMap<&str, usize> =
        entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut seen: HashSet<(usize, usize)> = Default::default();
    let mut edges = Vec::new();
    for (from, to) in links {
        let (Some(&a), Some(&b)) = (index.get(from.as_str()), index.get(to.as_str())) else {
            continue;
        };
        if a == b {
            continue;
        }
        let key = edge_key(a, b);
        if seen.insert(key) {
            edges.push(key);
        }
    }
    edges
}

fn link_degrees(entries: &[BrainEntry], edges: &[(usize, usize)]) -> Vec<u32> {
    let mut deg = vec![0u32; entries.len()];
    for &(a, b) in edges {
        if let Some(d) = deg.get_mut(a) { *d += 1; }
        if let Some(d) = deg.get_mut(b) { *d += 1; }
    }
    deg
}

/// Entries after kind + repo + tag + search filtering, in the requested
/// sort order. `edges`/`sort` only matter for `BrainSort::MostLinked`
/// (degree computed against the *full* `entries` set, not the filtered
/// subset, so "most linked" reflects real global link counts).
pub fn visible_entries<'a>(
    entries: &'a [BrainEntry],
    edges: &[(usize, usize)],
    kind: Option<&str>,
    repo: Option<&str>,
    tag: Option<&str>,
    query: &str,
    sort: BrainSort,
) -> Vec<&'a BrainEntry> {
    let degrees = (sort == BrainSort::MostLinked).then(|| link_degrees(entries, edges));

    let mut indexed: Vec<(usize, &BrainEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| kind.is_none_or(|k| normalize_kind(&e.entry_type) == k))
        .filter(|(_, e)| repo.is_none_or(|r| e.repos.iter().any(|x| x == r)))
        .filter(|(_, e)| tag.is_none_or(|t| e.tags.iter().any(|x| x == t)))
        .filter(|(_, e)| matches_search(e, query))
        .collect();

    match sort {
        BrainSort::Recent => indexed.sort_by(|a, b| b.1.updated.cmp(&a.1.updated)),
        BrainSort::Az => indexed.sort_by(|a, b| a.1.name.cmp(&b.1.name)),
        BrainSort::MostLinked => {
            let degrees = degrees.unwrap_or_default();
            indexed.sort_by(|a, b| {
                degrees.get(b.0).unwrap_or(&0).cmp(degrees.get(a.0).unwrap_or(&0))
                    .then_with(|| a.1.name.cmp(&b.1.name))
            });
        }
    }
    indexed.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn entry(id: &str, ty: &str, name: &str) -> BrainEntry {
        BrainEntry { id: id.into(), entry_type: ty.into(), name: name.into(), tags: vec![], repos: vec![], updated: None, body: String::new(), source: String::new() }
    }

    #[test]
    fn kinds_sorted_by_count_merges_duplicate_rows() {
        let entries = vec![
            entry("decisions/a.md", "decision", "A"),
            entry("decisions/b.md", "decisions", "B"),
            entry("decisions/c.md", "decisions", "C"),
            entry("errors/x.md", "error", "X"),
        ];
        let kinds = kinds_sorted_by_count(&entries);
        assert_eq!(kinds, vec![
            ("decisions".to_string(), "Decisions".to_string(), 3),
            ("errors".to_string(), "Errors".to_string(), 1),
        ]);
    }

    #[test]
    fn kinds_sorted_by_count_ties_break_alphabetically() {
        let entries = vec![entry("a.md", "errors", "A"), entry("b.md", "decisions", "B")];
        let kinds = kinds_sorted_by_count(&entries);
        assert_eq!(kinds[0].0, "decisions");
        assert_eq!(kinds[1].0, "errors");
    }

    #[test]
    fn kinds_sorted_by_count_on_empty_entries_is_empty() {
        assert!(kinds_sorted_by_count(&[]).is_empty());
    }

    #[test]
    fn visible_entries_filters_by_kind() {
        let entries = vec![entry("a.md", "decision", "A"), entry("b.md", "error", "B")];
        let out = visible_entries(&entries, &[], Some("decisions"), None, None, "", BrainSort::Recent);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a.md");
    }

    #[test]
    fn visible_entries_filters_by_repo_and_tag() {
        let mut a = entry("a.md", "decision", "A");
        a.repos = vec!["ninox".to_string()];
        a.tags = vec!["lifecycle".to_string()];
        let mut b = entry("b.md", "decision", "B");
        b.repos = vec!["bellhop".to_string()];
        let entries = vec![a, b];

        assert_eq!(visible_entries(&entries, &[], None, Some("ninox"), None, "", BrainSort::Recent).len(), 1);
        assert_eq!(visible_entries(&entries, &[], None, None, Some("lifecycle"), "", BrainSort::Recent).len(), 1);
        assert_eq!(visible_entries(&entries, &[], None, Some("bellhop"), Some("lifecycle"), "", BrainSort::Recent).len(), 0);
    }

    #[test]
    fn visible_entries_filters_by_search_across_name_tags_body() {
        let mut a = entry("a.md", "decision", "Auto-reap");
        a.body = "something about reaping".to_string();
        let b = entry("b.md", "decision", "Other");
        let entries = vec![a, b];
        let out = visible_entries(&entries, &[], None, None, None, "reap", BrainSort::Recent);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a.md");
    }

    #[test]
    fn visible_entries_az_sorts_by_name() {
        let entries = vec![entry("a.md", "x", "Zebra"), entry("b.md", "x", "Apple")];
        let out = visible_entries(&entries, &[], None, None, None, "", BrainSort::Az);
        assert_eq!(out[0].name, "Apple");
        assert_eq!(out[1].name, "Zebra");
    }

    #[test]
    fn visible_entries_most_linked_sorts_by_global_degree() {
        let entries = vec![
            entry("a.md", "x", "A"),
            entry("b.md", "x", "B"),
            entry("c.md", "x", "C"),
        ];
        // b <-> a and b <-> c: b has degree 2, a and c have degree 1 each.
        let edges = vec![(0, 1), (1, 2)];
        let out = visible_entries(&entries, &edges, None, None, None, "", BrainSort::MostLinked);
        assert_eq!(out[0].id, "b.md");
    }

    #[test]
    fn visible_entries_most_linked_with_no_edges_keeps_both_entries() {
        let entries = vec![entry("a.md", "x", "Zebra"), entry("b.md", "x", "Apple")];
        let out = visible_entries(&entries, &[], None, None, None, "", BrainSort::MostLinked);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn count_for_kind_ignores_repo_tag_search() {
        let entries = vec![entry("a.md", "decision", "A"), entry("b.md", "decision", "B"), entry("c.md", "error", "C")];
        assert_eq!(count_for_kind(&entries, Some("decisions")), 2);
        assert_eq!(count_for_kind(&entries, None), 3);
    }

    #[test]
    fn top_values_orders_by_count_then_alphabetically_and_truncates() {
        let mut a = entry("a.md", "x", "A");
        a.repos = vec!["ninox".to_string()];
        let mut b = entry("b.md", "x", "B");
        b.repos = vec!["ninox".to_string()];
        let mut c = entry("c.md", "x", "C");
        c.repos = vec!["bellhop".to_string()];
        let mut d = entry("d.md", "x", "D");
        d.repos = vec!["archipelago".to_string()];
        let entries = [a, b, c, d];
        let (top, total) = top_values(entries.iter(), |e| &e.repos, 2);
        assert_eq!(total, 3);
        assert_eq!(top, vec![("ninox".to_string(), 2), ("archipelago".to_string(), 1)]);
    }

    #[test]
    fn resolve_edges_dedups_mutual_links() {
        let entries = vec![entry("a.md", "x", "A"), entry("b.md", "x", "B")];
        let links = vec![("a.md".to_string(), "b.md".to_string()), ("b.md".to_string(), "a.md".to_string())];
        assert_eq!(resolve_edges(&entries, &links), vec![(0, 1)]);
    }

    #[test]
    fn resolve_edges_skips_unresolvable_ids() {
        let entries = vec![entry("a.md", "x", "A"), entry("b.md", "x", "B")];
        let links = vec![
            ("a.md".to_string(), "b.md".to_string()),
            ("a.md".to_string(), "missing.md".to_string()),
        ];
        assert_eq!(resolve_edges(&entries, &links), vec![(0, 1)]);
    }

    #[test]
    fn resolve_edges_drops_self_links() {
        let entries = vec![entry("a.md", "x", "A")];
        let links = vec![("a.md".to_string(), "a.md".to_string())];
        assert!(resolve_edges(&entries, &links).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Graph model — shared by the GUI's Brain Graph view and the TUI's Brain
// graph view. Only the pure selection/counting logic lives here; layout,
// hit-testing and rendering are view-specific (the GUI lays out a
// pixel-space force simulation, the TUI a terminal-native rataflow grid) and
// stay with their own app.
// ---------------------------------------------------------------------------

/// Breadth-first search from `focus` over the undirected `edges` (index
/// pairs into the entry set `edges` was resolved against — see
/// `resolve_edges`), collecting every node within `hops` steps. Always
/// includes `focus` itself, even with no edges at all. `hops == 0` returns
/// just `{focus}`.
pub fn neighbourhood(focus: usize, edges: &[(usize, usize)], hops: u8) -> HashSet<usize> {
    let mut adjacency: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(a, b) in edges {
        adjacency.entry(a).or_default().push(b);
        adjacency.entry(b).or_default().push(a);
    }

    let mut visited: HashSet<usize> = HashSet::from([focus]);
    let mut frontier: std::collections::VecDeque<(usize, u8)> = std::collections::VecDeque::from([(focus, 0)]);
    while let Some((node, depth)) = frontier.pop_front() {
        if depth >= hops {
            continue;
        }
        if let Some(neighbors) = adjacency.get(&node) {
            for &n in neighbors {
                if visited.insert(n) {
                    frontier.push_back((n, depth + 1));
                }
            }
        }
    }
    visited
}

/// Past this many hop-2-and-beyond nodes, a well-connected hub's
/// Neighbourhood is a hairball rather than a graph — `neighbourhood_capped`
/// keeps it legible by keeping only the highest-degree ones.
const HOP1_PLUS_DENSE_THRESHOLD: usize = 40;
/// How many of the highest-degree beyond-hop-1 nodes survive the cap.
const HOP1_PLUS_KEEP_TOP_N: usize = 15;

/// `neighbourhood`, capped for legibility: if the nodes beyond hop 1 number
/// more than `HOP1_PLUS_DENSE_THRESHOLD`, keep every hop-0/1 node plus only
/// the `HOP1_PLUS_KEEP_TOP_N` highest-degree ones beyond that — ranked by
/// degree across the full edge set, not just within this already-capped
/// subgraph. Returns the node ids to show and how many were dropped, so the
/// UI can show a "+N more" hint instead of silently cutting them.
pub fn neighbourhood_capped(focus: usize, edges: &[(usize, usize)], hops: u8) -> (Vec<usize>, usize) {
    let full = neighbourhood(focus, edges, hops);
    let one_hop = neighbourhood(focus, edges, 1.min(hops));
    let beyond: Vec<usize> = full.iter().copied().filter(|n| !one_hop.contains(n)).collect();

    if beyond.len() <= HOP1_PLUS_DENSE_THRESHOLD {
        let mut ids: Vec<usize> = full.into_iter().collect();
        ids.sort_unstable();
        return (ids, 0);
    }

    let mut degree: HashMap<usize, usize> = HashMap::new();
    for &(a, b) in edges {
        *degree.entry(a).or_default() += 1;
        *degree.entry(b).or_default() += 1;
    }
    let mut ranked = beyond.clone();
    ranked.sort_by(|a, b| degree.get(b).copied().unwrap_or(0).cmp(&degree.get(a).copied().unwrap_or(0)).then(a.cmp(b)));
    let kept_count = ranked.len().min(HOP1_PLUS_KEEP_TOP_N);
    let dropped = beyond.len() - kept_count;

    let mut ids: Vec<usize> = one_hop.into_iter().chain(ranked.into_iter().take(kept_count)).collect();
    ids.sort_unstable();
    (ids, dropped)
}

/// One cluster bubble (per repo): `label` the repo name (or `"(no repo)"`
/// for entries with none), `count` its entry count, `(x, y)` its centre and
/// `radius` in arbitrary layout units, sized so `radius` grows with the
/// square root of `count` (area, not radius, proportional to count).
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterBubble {
    pub label: String,
    pub count: usize,
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    /// The most common `normalize_kind` value among this repo's entries —
    /// drives the bubble's color/legend so same-sized bubbles aren't
    /// visually identical. Ties break alphabetically for determinism.
    pub dominant_kind: String,
}

/// One cluster per repo, sized by count and circle-packed around the
/// largest, deterministically.
pub fn cluster_by_repo(entries: &[BrainEntry]) -> Vec<ClusterBubble> {
    const NO_REPO: &str = "(no repo)";
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut kind_counts: HashMap<&str, HashMap<String, usize>> = HashMap::new();
    for e in entries {
        let kind = normalize_kind(&e.entry_type);
        if e.repos.is_empty() {
            *counts.entry(NO_REPO).or_default() += 1;
            *kind_counts.entry(NO_REPO).or_default().entry(kind.clone()).or_default() += 1;
        } else {
            for r in &e.repos {
                *counts.entry(r.as_str()).or_default() += 1;
                *kind_counts.entry(r.as_str()).or_default().entry(kind.clone()).or_default() += 1;
            }
        }
    }
    let mut sorted: Vec<(&str, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let radius_of = |count: usize| (count as f32).sqrt() * 0.08 + 0.05;
    const MARGIN: f32 = 0.06;
    // Greedy circle packing, largest first: each bubble goes to the spot
    // nearest the centre that touches an already-placed bubble without
    // overlapping any, so the clusters form one round, packed group rather
    // than rows.
    const ANGLES: usize = 36;
    let mut placed: Vec<(f32, f32, f32)> = Vec::with_capacity(sorted.len());
    let mut bubbles = Vec::with_capacity(sorted.len());
    for (label, count) in sorted {
        let r = radius_of(count);
        let (x, y) = if placed.is_empty() {
            (0.0, 0.0)
        } else {
            let mut best: Option<(f32, f32, f32)> = None;
            for &(px, py, pr) in &placed {
                let d = pr + r + MARGIN;
                for k in 0..ANGLES {
                    let a = k as f32 / ANGLES as f32 * std::f32::consts::TAU;
                    let (cx, cy) = (px + d * a.cos(), py + d * a.sin());
                    let dist0 = cx.hypot(cy);
                    if best.is_some_and(|(_, _, bd)| dist0 >= bd) {
                        continue;
                    }
                    let clear = placed.iter().all(|&(qx, qy, qr)| (cx - qx).hypot(cy - qy) >= qr + r + MARGIN - 1e-4);
                    if clear {
                        best = Some((cx, cy, dist0));
                    }
                }
            }
            best.map(|(x, y, _)| (x, y)).unwrap_or((0.0, 0.0))
        };
        placed.push((x, y, r));
        let dominant_kind = kind_counts
            .get(label)
            .map(|kinds| {
                let mut by_count: Vec<(&String, &usize)> = kinds.iter().collect();
                by_count.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                by_count.first().map(|(k, _)| (*k).clone()).unwrap_or_default()
            })
            .unwrap_or_default();
        bubbles.push(ClusterBubble { label: label.to_string(), count, x, y, radius: r, dominant_kind });
    }
    bubbles
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    fn entry(id: &str) -> BrainEntry {
        BrainEntry { id: id.into(), entry_type: "concepts".into(), name: id.into(), tags: vec![], repos: vec![], updated: None, body: String::new(), source: String::new() }
    }

    #[test]
    fn neighbourhood_always_includes_the_focus() {
        let n = neighbourhood(5, &[], 2);
        assert_eq!(n, HashSet::from([5]));
    }

    #[test]
    fn neighbourhood_zero_hops_is_just_the_focus() {
        let edges = [(0, 1), (1, 2)];
        assert_eq!(neighbourhood(0, &edges, 0), HashSet::from([0]));
    }

    #[test]
    fn neighbourhood_one_hop_is_direct_links_only() {
        let edges = [(0, 1), (1, 2)];
        assert_eq!(neighbourhood(0, &edges, 1), HashSet::from([0, 1]));
    }

    #[test]
    fn neighbourhood_two_hops_reaches_a_chain() {
        let edges = [(0, 1), (1, 2), (2, 3)];
        assert_eq!(neighbourhood(0, &edges, 2), HashSet::from([0, 1, 2]));
    }

    #[test]
    fn neighbourhood_does_not_revisit_through_a_cycle() {
        let edges = [(0, 1), (1, 2), (2, 0)];
        assert_eq!(neighbourhood(0, &edges, 5), HashSet::from([0, 1, 2]));
    }

    #[test]
    fn neighbourhood_capped_is_unchanged_below_the_dense_threshold() {
        let edges = [(0, 1), (1, 2), (2, 3)];
        let (ids, dropped) = neighbourhood_capped(0, &edges, 2);
        assert_eq!(dropped, 0);
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn neighbourhood_capped_keeps_hop1_fully_and_caps_the_rest_by_degree() {
        // A hub (0) with 60 direct neighbours, each with one further
        // (distinct) hop-2 neighbour — 60 hop-2 candidates, well past
        // `HOP1_PLUS_DENSE_THRESHOLD`. Node 1's extra neighbour (100) also
        // links to two more hop-2 nodes, giving it the highest degree among
        // the hop-2 set, so it must survive the cap.
        let mut edges: Vec<(usize, usize)> = (1..=60).map(|i| (0, i)).collect();
        for i in 1..=60 {
            edges.push((i, 100 + i));
        }
        edges.push((101, 200));
        edges.push((101, 201));

        let (ids, dropped) = neighbourhood_capped(0, &edges, 2);
        assert_eq!(dropped, 60 - HOP1_PLUS_KEEP_TOP_N);
        for hop1 in 1..=60 {
            assert!(ids.contains(&hop1), "every hop-1 node must survive the cap: {hop1}");
        }
        assert!(ids.contains(&101), "the highest-degree hop-2 node must survive the cap");
        assert_eq!(ids.len(), 1 /* focus */ + 60 /* hop-1 */ + HOP1_PLUS_KEEP_TOP_N);
    }

    #[test]
    fn cluster_by_repo_counts_and_sorts_descending() {
        let mut a = entry("a.md");
        a.repos = vec!["ninox".to_string()];
        let mut b = entry("b.md");
        b.repos = vec!["ninox".to_string()];
        let mut c = entry("c.md");
        c.repos = vec!["bellhop".to_string()];
        let bubbles = cluster_by_repo(&[a, b, c]);
        assert_eq!(bubbles[0].label, "ninox");
        assert_eq!(bubbles[0].count, 2);
        assert_eq!(bubbles[1].label, "bellhop");
        assert_eq!(bubbles[1].count, 1);
    }

    #[test]
    fn cluster_by_repo_groups_repo_less_entries_together() {
        let bubbles = cluster_by_repo(&[entry("a.md"), entry("b.md")]);
        assert_eq!(bubbles.len(), 1);
        assert_eq!(bubbles[0].count, 2);
    }

    #[test]
    fn cluster_by_repo_counts_a_multi_repo_entry_in_each_of_its_repos() {
        let mut a = entry("a.md");
        a.repos = vec!["ninox".to_string(), "bellhop".to_string()];
        let bubbles = cluster_by_repo(&[a]);
        assert_eq!(bubbles.len(), 2);
        assert!(bubbles.iter().all(|b| b.count == 1));
    }

    #[test]
    fn cluster_by_repo_picks_the_most_common_kind_as_dominant() {
        let mut a = entry("a.md");
        a.repos = vec!["ninox".to_string()];
        a.entry_type = "decisions".into();
        let mut b = entry("b.md");
        b.repos = vec!["ninox".to_string()];
        b.entry_type = "decisions".into();
        let mut c = entry("c.md");
        c.repos = vec!["ninox".to_string()];
        c.entry_type = "errors".into();
        let bubbles = cluster_by_repo(&[a, b, c]);
        assert_eq!(bubbles[0].label, "ninox");
        assert_eq!(bubbles[0].dominant_kind, "decisions");
    }

    #[test]
    fn an_untyped_entry_takes_its_top_level_directory_as_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let write = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            p
        };
        let nested = write("repos/ninox/session-limits.md", "---\ntitle: x\n---\nbody\n");
        let typed = write("repos/ninox/typed.md", "---\ntype: decision\n---\nbody\n");
        let root = write("loose.md", "body\n");
        assert_eq!(process_file(dir.path(), &nested).unwrap().entry_type, "repos", "not the repo subfolder's name");
        assert_eq!(process_file(dir.path(), &typed).unwrap().entry_type, "decision", "frontmatter still wins");
        assert_eq!(process_file(dir.path(), &root).unwrap().entry_type, "note");
    }

    #[test]
    fn clusters_pack_into_a_round_group_not_a_column() {
        let entries: Vec<BrainEntry> = (0..400)
            .map(|i| {
                let mut e = entry(&format!("e{i}.md"));
                // Skewed like a real brain: one big repo, many small ones.
                e.repos = vec![if i < 200 { "big".to_string() } else { format!("repo-{}", i % 40) }];
                e
            })
            .collect();
        let bubbles = cluster_by_repo(&entries);
        let (min_x, max_x) = bubbles.iter().fold((f32::MAX, f32::MIN), |(lo, hi), b| (lo.min(b.x - b.radius), hi.max(b.x + b.radius)));
        let (min_y, max_y) = bubbles.iter().fold((f32::MAX, f32::MIN), |(lo, hi), b| (lo.min(b.y - b.radius), hi.max(b.y + b.radius)));
        let aspect = (max_x - min_x) / (max_y - min_y);
        assert!((0.6..=1.7).contains(&aspect), "packed clusters should be roughly round, got aspect {aspect}");
        assert_eq!(cluster_by_repo(&entries), bubbles, "deterministic");
    }

    #[test]
    fn cluster_bubbles_never_overlap() {
        let entries: Vec<BrainEntry> = (0..20)
            .map(|i| {
                let mut e = entry(&format!("e{i}.md"));
                e.repos = vec![format!("repo-{}", i % 7)];
                e
            })
            .collect();
        let bubbles = cluster_by_repo(&entries);
        for i in 0..bubbles.len() {
            for j in (i + 1)..bubbles.len() {
                let dist = (bubbles[i].x - bubbles[j].x).hypot(bubbles[i].y - bubbles[j].y);
                assert!(
                    dist >= bubbles[i].radius + bubbles[j].radius - 0.001,
                    "bubbles {i} and {j} overlap: dist {dist}, radii {} + {}",
                    bubbles[i].radius, bubbles[j].radius
                );
            }
        }
    }
}

/// Result of a `BrainIndex::rebuild()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildStats {
    /// Entries (files) indexed this run.
    pub indexed: usize,
    /// Entries newly embedded this run (content changed or never embedded).
    pub embedded: usize,
    /// Entries whose cached embedding was reused because content is unchanged.
    pub cached: usize,
}

// ---------------------------------------------------------------------------
// BrainIndex
// ---------------------------------------------------------------------------

pub struct BrainIndex {
    conn: Mutex<Connection>,
    brain_path: PathBuf,
}

// Manual impl because the GUI's `Message` enum derives `Debug` and carries
// an `Arc<BrainIndex>` (background freshen results); the SQLite connection
// has no useful debug form, so show only the identifying path.
impl std::fmt::Debug for BrainIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrainIndex").field("brain_path", &self.brain_path).finish_non_exhaustive()
    }
}

impl BrainIndex {
    pub fn open(brain_path: impl AsRef<Path>) -> Result<Self> {
        let brain_path = brain_path.as_ref().to_path_buf();
        fs::create_dir_all(&brain_path)
            .with_context(|| format!("create brain dir {brain_path:?}"))?;
        let db_path = brain_path.join(".index.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open brain db {db_path:?}"))?;
        conn.execute_batch(
            // busy_timeout: the GUI's background freshen/reindex, the
            // in-process server's per-read freshen, and CLI invocations can
            // rebuild through *separate connections* to this DB; without a
            // timeout the losing writer fails SQLITE_BUSY instantly instead
            // of waiting out the other's transaction (same setting as
            // store.rs).
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS entries (
                 id      TEXT PRIMARY KEY,
                 type    TEXT NOT NULL,
                 name    TEXT NOT NULL,
                 tags    TEXT NOT NULL DEFAULT '[]',
                 repos   TEXT NOT NULL DEFAULT '[]',
                 updated TEXT,
                 body    TEXT NOT NULL DEFAULT ''
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts
                 USING fts5(name, tags, body, content=entries, content_rowid=rowid);
             CREATE TABLE IF NOT EXISTS links (from_id TEXT NOT NULL, target TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS links_from ON links(from_id);
             CREATE INDEX IF NOT EXISTS links_target ON links(target);
             CREATE TABLE IF NOT EXISTS embeddings (
                 id           TEXT PRIMARY KEY,
                 content_hash INTEGER NOT NULL,
                 vector       BLOB NOT NULL
             );",
        )?;
        // Expose `stem(x)` to SQL so link-resolution queries can share the
        // same "stem(target) == stem(id)" rule used in Rust, without
        // round-tripping candidate sets through the application layer.
        conn.create_scalar_function(
            "stem",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let s: String = ctx.get(0)?;
                Ok(stem_of(&s))
            },
        )?;
        ensure_gitignore(&brain_path)?;
        Ok(Self { conn: Mutex::new(conn), brain_path })
    }

    /// The brain directory this index was opened on.
    pub fn path(&self) -> &Path {
        &self.brain_path
    }

    /// Walk the brain directory, parse markdown files, and repopulate the
    /// index, then embed any new or changed entries (skipped entirely if
    /// `embedder` is `None`, or if embedding a given entry fails — indexing
    /// is never blocked by the embedding step).
    pub fn rebuild(&self, embedder: Option<&dyn Embedder>) -> Result<RebuildStats> {
        // Cheap sequential walk to enumerate candidate files.
        let paths: Vec<PathBuf> = WalkDir::new(&self.brain_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md"))
            .filter(|p| {
                let rel = p.strip_prefix(&self.brain_path).unwrap_or(p).to_string_lossy();
                !crate::brain_rules::is_reserved_rel(&rel)
            })
            .collect();

        // Read + parse across a thread pool. Pure function, no shared state.
        let records: Vec<FileRecord> = paths
            .par_iter()
            .filter_map(|p| process_file(&self.brain_path, p))
            .collect();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute_batch("DELETE FROM entries; DELETE FROM entries_fts; DELETE FROM links;")?;

        let indexed = records.len();
        {
            let mut insert_entry = tx.prepare(
                "INSERT INTO entries (id, type, name, tags, repos, updated, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut insert_link =
                tx.prepare("INSERT INTO links (from_id, target) VALUES (?1, ?2)")?;

            for rec in &records {
                let tags_json = serde_json::to_string(&rec.tags)?;
                let repos_json = serde_json::to_string(&rec.repos)?;
                insert_entry.execute(params![
                    rec.id,
                    rec.entry_type,
                    rec.name,
                    tags_json,
                    repos_json,
                    rec.updated,
                    rec.body
                ])?;
                for target in &rec.links {
                    insert_link.execute(params![rec.id, target])?;
                }
            }
        }
        tx.commit()?;

        // Rebuild the FTS index from the content table.
        conn.execute_batch("INSERT INTO entries_fts(entries_fts) VALUES('rebuild');")?;

        let (embedded, cached) = match embedder {
            Some(embedder) => Self::sync_embeddings(&conn, &records, embedder)?,
            None => (0, 0),
        };

        Ok(RebuildStats { indexed, embedded, cached })
    }

    /// Compute-or-reuse an embedding for every current record, then prune
    /// embeddings for entries that no longer exist. Returns
    /// `(newly_embedded, reused_from_cache)`. A failure embedding any single
    /// batch is logged and treated as "not embedded" rather than
    /// propagated — a broken model must never break `rebuild()`.
    fn sync_embeddings(
        conn: &Connection,
        records: &[FileRecord],
        embedder: &dyn Embedder,
    ) -> Result<(usize, usize)> {
        let mut cached_hashes: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT id, content_hash FROM embeddings")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (id, hash) = row?;
                cached_hashes.insert(id, hash);
            }
        }

        let mut to_embed: Vec<(&FileRecord, String, i64)> = Vec::new();
        let mut cached = 0usize;
        for rec in records {
            let text = embedding_text(rec);
            let hash = content_hash(&text);
            if cached_hashes.get(&rec.id) == Some(&hash) {
                cached += 1;
            } else {
                to_embed.push((rec, text, hash));
            }
        }

        let embedded = if to_embed.is_empty() {
            0
        } else {
            let texts: Vec<String> = to_embed.iter().map(|(_, text, _)| text.clone()).collect();
            match embedder.embed_batch(&texts) {
                Ok(vectors) => {
                    let mut upsert = conn.prepare(
                        "INSERT INTO embeddings (id, content_hash, vector) VALUES (?1, ?2, ?3)
                         ON CONFLICT(id) DO UPDATE SET content_hash = excluded.content_hash, vector = excluded.vector",
                    )?;
                    let mut upserted = 0usize;
                    for ((rec, _, hash), vector) in to_embed.iter().zip(vectors.iter()) {
                        upsert.execute(params![rec.id, hash, vector_to_blob(vector)])?;
                        upserted += 1;
                    }
                    upserted
                }
                Err(err) => {
                    tracing::warn!("brain: failed to embed {} entries: {err}", to_embed.len());
                    0
                }
            }
        };

        // Prune embeddings for entries that no longer exist.
        let live_ids: HashSet<&str> = records.iter().map(|r| r.id.as_str()).collect();
        let mut stale: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT id FROM embeddings")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let id = row?;
                if !live_ids.contains(id.as_str()) {
                    stale.push(id);
                }
            }
        }
        if !stale.is_empty() {
            let mut delete = conn.prepare("DELETE FROM embeddings WHERE id = ?1")?;
            for id in &stale {
                delete.execute(params![id])?;
            }
        }

        Ok((embedded, cached))
    }

    /// Hybrid full-text + semantic search. When `text` is non-empty and
    /// `embedder` is `Some`, blends the existing FTS5 keyword ranking with
    /// vector similarity via Reciprocal Rank Fusion. Falls back to exactly
    /// today's keyword-only (or filter-only) behavior when `text` is empty
    /// or `embedder` is `None`.
    pub fn query(
        &self,
        text: &str,
        embedder: Option<&dyn Embedder>,
        filters: QueryFilters,
    ) -> Result<Vec<BrainEntry>> {
        let conn = self.conn.lock().unwrap();

        if text.trim().is_empty() && filters.entry_type.is_none() && filters.tag.is_none() {
            // Return all entries when no constraints given
            let mut stmt = conn.prepare(
                "SELECT id, type, name, tags, repos, updated, body FROM entries ORDER BY name",
            )?;
            let rows = stmt.query_map([], row_to_entry)?;
            let entries: Vec<BrainEntry> =
                rows.collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(entries);
        }

        if text.trim().is_empty() {
            // Filter-only query
            let mut stmt = conn.prepare(
                "SELECT id, type, name, tags, repos, updated, body FROM entries
                 WHERE (?1 IS NULL OR type = ?1)
                 ORDER BY name",
            )?;
            let rows = stmt.query_map(params![filters.entry_type.as_deref()], row_to_entry)?;
            let mut results: Vec<BrainEntry> =
                rows.collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some(ref tag) = filters.tag {
                results.retain(|e| e.tags.iter().any(|t| t == tag));
            }
            return Ok(results);
        }

        // Keyword leg: existing FTS5 ranking, as an ordered id list.
        let mut stmt = conn.prepare(
            "SELECT e.id, e.type, e.name, e.tags, e.repos, e.updated, e.body
             FROM entries_fts
             JOIN entries e ON entries_fts.rowid = e.rowid
             WHERE entries_fts MATCH ?1
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(params![sanitize_fts_query(text)], row_to_entry)?;
        let keyword_results: Vec<BrainEntry> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let keyword_ids: Vec<String> = keyword_results.iter().map(|e| e.id.clone()).collect();

        // Semantic leg: top-20 by cosine similarity, if an embedder is available.
        let semantic_ids: Vec<String> = match embedder {
            Some(embedder) => Self::semantic_candidates(&conn, embedder, text)?,
            None => Vec::new(),
        };

        let fusion_ran = !semantic_ids.is_empty();
        let mut results: Vec<BrainEntry> = if semantic_ids.is_empty() {
            keyword_results
        } else {
            let fused = reciprocal_rank_fusion(&[keyword_ids, semantic_ids], 60.0);
            let mut by_id: HashMap<String, BrainEntry> =
                keyword_results.into_iter().map(|e| (e.id.clone(), e)).collect();
            let mut fused_results = Vec::with_capacity(fused.len());
            for (id, _score) in fused {
                if let Some(entry) = by_id.remove(&id) {
                    fused_results.push(entry);
                } else if let Some(entry) = get_by_id(&conn, &id)? {
                    fused_results.push(entry);
                }
            }
            fused_results
        };

        // Post-filter by type and tag.
        if let Some(ref et) = filters.entry_type {
            results.retain(|e| &e.entry_type == et);
        }
        if let Some(ref tag) = filters.tag {
            results.retain(|e| e.tags.iter().any(|t| t == tag));
        }

        // Cap only applies when semantic fusion actually happened — the
        // no-embedder / no-semantic-contribution path stays exactly
        // today's uncapped keyword behavior, matching the design spec's
        // degradation guarantee.
        if fusion_ran {
            results.truncate(20);
        }
        Ok(results)
    }

    /// Top-20 entry ids by cosine similarity to `text`, embedded with the
    /// query-instruction prefix Arctic Embed expects for asymmetric
    /// retrieval. Returns an empty list (never an error) if embedding the
    /// query text fails — a broken embedder degrades to keyword-only
    /// results, it never fails the whole query.
    fn semantic_candidates(
        conn: &Connection,
        embedder: &dyn Embedder,
        text: &str,
    ) -> Result<Vec<String>> {
        let query_vector = match embedder.embed(&format!("{QUERY_INSTRUCTION_PREFIX}{text}")) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!("brain: failed to embed query text: {err}");
                return Ok(Vec::new());
            }
        };

        let mut stmt = conn.prepare("SELECT id, vector FROM embeddings")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;

        let mut scored: Vec<(String, f32)> = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            let vector = blob_to_vector(&blob);
            scored.push((id, cosine_similarity(&query_vector, &vector)));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(20);
        Ok(scored.into_iter().map(|(id, _)| id).collect())
    }

    /// Fetch a single entry by its relative path id.
    pub fn get(&self, id: &str) -> Result<Option<BrainEntry>> {
        let conn = self.conn.lock().unwrap();
        get_by_id(&conn, id)
    }

    /// Entries whose links resolve to `id` (i.e. entries that link *to* `id`).
    ///
    /// Resolution rule (matches the app's render-time wikilink resolution):
    /// a raw link target `t` resolves to entry `e` when
    /// `t == e.name OR t == e.id OR stem(t) == stem(e.id) OR stem(t) == e.name`.
    pub fn backlinks(&self, id: &str) -> Result<Vec<BrainEntry>> {
        let conn = self.conn.lock().unwrap();
        let Some(entry) = get_by_id(&conn, id)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT DISTINCT src.id, src.type, src.name, src.tags, src.repos, src.updated, src.body
             FROM links l
             JOIN entries src ON src.id = l.from_id
             WHERE src.id != ?2
               AND (l.target = ?1 OR l.target = ?2 OR stem(l.target) = stem(?2) OR stem(l.target) = ?1)
             ORDER BY src.name",
        )?;
        let rows = stmt.query_map(params![entry.name, entry.id], row_to_entry)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Resolved targets of `id`'s own links (i.e. entries that `id` links to).
    pub fn outlinks(&self, id: &str) -> Result<Vec<BrainEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT e.id, e.type, e.name, e.tags, e.repos, e.updated, e.body
             FROM links l
             JOIN entries e ON (
                 l.target = e.name OR
                 l.target = e.id OR
                 stem(l.target) = stem(e.id) OR
                 stem(l.target) = e.name
             )
             WHERE l.from_id = ?1 AND e.id != ?1
             ORDER BY e.name",
        )?;
        let rows = stmt.query_map(params![id], row_to_entry)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The full resolved (from_id, to_id) edge list, for pinboard rendering.
    pub fn links_all(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT l.from_id, e.id
             FROM links l
             JOIN entries e ON (
                 l.target = e.name OR
                 l.target = e.id OR
                 stem(l.target) = stem(e.id) OR
                 stem(l.target) = e.name
             )
             WHERE l.from_id != e.id
             ORDER BY l.from_id, e.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The raw (from_id, target) link pairs exactly as extracted from
    /// markdown, with no resolution against `entries` applied. Unlike
    /// `links_all`, this includes targets that don't resolve to any entry in
    /// *this* index at all -- `MergedBrainIndex` needs that to tell apart "no
    /// entry anywhere links here" from "the entry that resolves this link
    /// lives in a different catalogue".
    pub fn raw_links_all(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT from_id, target FROM links")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Entries related to `id`, ranked: direct links (either direction) first,
    /// then co-citation (entries reachable via a shared linked target or a
    /// shared linking source), then entries sharing at least one tag.
    /// Deduplicated, excludes `id` itself, capped at `limit`.
    pub fn related(&self, id: &str, limit: usize) -> Result<Vec<BrainEntry>> {
        let entry = match self.get(id)? {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut ranked: Vec<BrainEntry> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(entry.id.clone());

        // Tier 1: direct links, either direction.
        let outs = self.outlinks(id)?;
        let backs = self.backlinks(id)?;
        merge_unique(outs.clone(), &mut ranked, &mut seen);
        merge_unique(backs.clone(), &mut ranked, &mut seen);

        // Tier 2: co-citation -- entries that share an outbound target with
        // `id` (found via the backlinks of each of `id`'s targets), plus
        // entries reachable from the same sources that link to `id` (found
        // via the outlinks of each of `id`'s linking sources).
        let mut co_citation: Vec<BrainEntry> = Vec::new();
        for target in &outs {
            co_citation.extend(self.backlinks(&target.id)?);
        }
        for source in &backs {
            co_citation.extend(self.outlinks(&source.id)?);
        }
        merge_unique(co_citation, &mut ranked, &mut seen);

        // Tier 3: shared-tag overlap.
        if !entry.tags.is_empty() {
            let conn = self.conn.lock().unwrap();
            let clauses: Vec<&str> = entry.tags.iter().map(|_| "tags LIKE ?").collect();
            let sql = format!(
                "SELECT id, type, name, tags, repos, updated, body FROM entries
                 WHERE id != ? AND ({}) ORDER BY name",
                clauses.join(" OR ")
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut bind_params: Vec<String> = vec![entry.id.clone()];
            bind_params.extend(entry.tags.iter().map(|t| format!("%\"{t}\"%")));
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                bind_params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(param_refs.as_slice(), row_to_entry)?;
            let tag_matches: Vec<BrainEntry> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            merge_unique(tag_matches, &mut ranked, &mut seen);
        }

        ranked.truncate(limit);
        Ok(ranked)
    }
}

/// Merge `items` into `ranked`, skipping anything already present in `seen`.
fn merge_unique(items: Vec<BrainEntry>, ranked: &mut Vec<BrainEntry>, seen: &mut HashSet<String>) {
    for item in items {
        if seen.insert(item.id.clone()) {
            ranked.push(item);
        }
    }
}

/// Does raw wikilink target `target` resolve to `entry`? Pure-Rust twin of
/// the SQL join condition `backlinks`/`outlinks`/`links_all` use
/// (`l.target = e.name OR l.target = e.id OR stem(l.target) = stem(e.id) OR
/// stem(l.target) = e.name`) -- `MergedBrainIndex` needs the same rule as a
/// predicate it can apply across catalogue boundaries, where there's no
/// shared SQLite connection to run a join against.
pub fn link_target_matches(target: &str, entry: &BrainEntry) -> bool {
    target == entry.name
        || target == entry.id
        || stem_of(target) == stem_of(&entry.id)
        || stem_of(target) == entry.name
}

// ---------------------------------------------------------------------------
// MergedBrainIndex: fans a query out across every enabled catalogue
// ---------------------------------------------------------------------------

/// One catalogue's `BrainIndex`, labeled with the catalogue name it came
/// from. `MergedBrainIndex` opens one of these per *enabled* `CatalogueRef`
/// -- a disabled catalogue (`CatalogueRef.enabled == false`, the Sources
/// view's "Include in search" toggle) is simply never opened, so it can
/// never appear under "All sources".
#[derive(Debug, Clone)]
pub struct MergedSource {
    pub name: String,
    pub index: std::sync::Arc<BrainIndex>,
}

/// A read-only union of every enabled catalogue's `BrainIndex`, for the
/// GUI/TUI's "All sources" view. Each underlying `BrainIndex` is completely
/// unmodified -- same schema, same per-catalogue id space -- so direct
/// single-catalogue use (the CLI, the TUI, the server, and the GUI's normal
/// single-source view) is unaffected by this type's existence.
///
/// Entries returned through `MergedBrainIndex` have their `id` qualified as
/// `"<source>:<id>"` (see `qualify`/`split_qualified`) so the same relative
/// path in two different catalogues can't collide, and their `source` field
/// stamped with the owning catalogue's name -- both done here, in memory,
/// rather than persisted by `BrainIndex` itself.
#[derive(Debug)]
pub struct MergedBrainIndex {
    sources: Vec<MergedSource>,
}

/// Every source's full entry list and raw (unresolved) link list, fetched
/// once and shared across every lookup within a single top-level
/// `MergedBrainIndex` call. `backlinks`/`outlinks`/`links_all`/`related` all
/// need "every other source's entries" to find cross-source matches, and
/// `related`'s co-citation tier calls `backlinks`/`outlinks` once per link
/// the entry has -- without this cache each of those nested calls would
/// re-run a full `query("")` + `raw_links_all()` over every other source
/// from scratch.
struct MergeContext {
    entries: HashMap<String, Vec<BrainEntry>>,
    raw_links: HashMap<String, Vec<(String, String)>>,
}

impl MergeContext {
    fn build(sources: &[MergedSource]) -> Result<Self> {
        let mut entries = HashMap::new();
        let mut raw_links = HashMap::new();
        for src in sources {
            entries.insert(src.name.clone(), src.index.query("", None, QueryFilters::default())?);
            raw_links.insert(src.name.clone(), src.index.raw_links_all()?);
        }
        Ok(Self { entries, raw_links })
    }
}

impl MergedBrainIndex {
    /// Builds a merge directly from already-opened sources -- e.g. a
    /// caller that freshened each catalogue itself (pulling a remote
    /// before opening it) and just needs them wrapped into a merge.
    pub fn from_sources(sources: Vec<MergedSource>) -> Self {
        Self { sources }
    }

    /// Opens one `BrainIndex` per `enabled` catalogue. A catalogue that
    /// fails to open (e.g. a remote source mid-error) is logged and skipped
    /// rather than failing the whole merge -- one broken source shouldn't
    /// blank the "All sources" view for every other one.
    pub fn open(catalogues: &[crate::config::CatalogueRef]) -> Result<Self> {
        let mut sources = Vec::new();
        for cat in catalogues {
            if !cat.enabled {
                continue;
            }
            match BrainIndex::open(&cat.path) {
                Ok(index) => {
                    sources.push(MergedSource { name: cat.name.clone(), index: std::sync::Arc::new(index) })
                }
                Err(err) => tracing::warn!("merged brain: skipping catalogue '{}': {err}", cat.name),
            }
        }
        Ok(Self { sources })
    }

    /// The catalogues actually loaded (enabled and opened successfully).
    pub fn sources(&self) -> &[MergedSource] {
        &self.sources
    }

    pub fn qualify(source: &str, id: &str) -> String {
        format!("{source}:{id}")
    }

    /// Splits a merged id back into `(source, raw_id)`. Splits on the first
    /// `:` -- catalogue names are short config-chosen identifiers and are
    /// not expected to contain one.
    pub fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
        qualified.split_once(':')
    }

    fn stamp(source: &str, mut entry: BrainEntry) -> BrainEntry {
        entry.id = Self::qualify(source, &entry.id);
        entry.source = source.to_string();
        entry
    }

    fn resolves_within(entries: &[BrainEntry], target: &str) -> bool {
        entries.iter().any(|e| link_target_matches(target, e))
    }

    /// The first entry (in `self.sources` order) in any catalogue other
    /// than `exclude` whose identity matches raw link target `target`.
    fn resolve_cross_source(&self, ctx: &MergeContext, exclude: &str, target: &str) -> Option<(String, BrainEntry)> {
        for other in &self.sources {
            if other.name == exclude {
                continue;
            }
            let Some(entries) = ctx.entries.get(&other.name) else { continue };
            if let Some(hit) = entries.iter().find(|e| link_target_matches(target, e)) {
                return Some((other.name.clone(), hit.clone()));
            }
        }
        None
    }

    /// Hybrid full-text + semantic search, merged across every source. With
    /// no search text, each source's own "all entries" list is concatenated
    /// and re-sorted by name (matching a single `BrainIndex`'s own ordering
    /// for that case). With search text, each source's own ranked id list is
    /// fused with the existing `reciprocal_rank_fusion` helper -- the same
    /// mechanism a single `BrainIndex` already uses to blend its keyword and
    /// semantic legs -- so a near-top result in a small catalogue doesn't
    /// automatically outrank a true top hit in a larger one.
    pub fn query(&self, text: &str, embedder: Option<&dyn Embedder>, filters: QueryFilters) -> Result<Vec<BrainEntry>> {
        if text.trim().is_empty() {
            let mut merged = Vec::new();
            for src in &self.sources {
                let entries = src.index.query(
                    text,
                    embedder,
                    QueryFilters { entry_type: filters.entry_type.clone(), tag: filters.tag.clone() },
                )?;
                merged.extend(entries.into_iter().map(|e| Self::stamp(&src.name, e)));
            }
            merged.sort_by(|a, b| a.name.cmp(&b.name));
            return Ok(merged);
        }

        let mut by_qid: HashMap<String, BrainEntry> = HashMap::new();
        let mut ranked_lists: Vec<Vec<String>> = Vec::new();
        for src in &self.sources {
            let entries = src.index.query(
                text,
                embedder,
                QueryFilters { entry_type: filters.entry_type.clone(), tag: filters.tag.clone() },
            )?;
            let mut ids = Vec::with_capacity(entries.len());
            for e in entries {
                let stamped = Self::stamp(&src.name, e);
                ids.push(stamped.id.clone());
                by_qid.insert(stamped.id.clone(), stamped);
            }
            ranked_lists.push(ids);
        }
        let fused = reciprocal_rank_fusion(&ranked_lists, 60.0);
        let mut results = Vec::with_capacity(fused.len());
        for (id, _score) in fused {
            if let Some(e) = by_qid.remove(&id) {
                results.push(e);
            }
        }
        Ok(results)
    }

    /// Fetch a single entry by its merged (`"source:id"`) id.
    pub fn get(&self, qualified_id: &str) -> Result<Option<BrainEntry>> {
        let Some((source, raw_id)) = Self::split_qualified(qualified_id) else { return Ok(None) };
        let Some(src) = self.sources.iter().find(|s| s.name == source) else { return Ok(None) };
        Ok(src.index.get(raw_id)?.map(|e| Self::stamp(source, e)))
    }

    /// Entries linking to `qualified_id`, same-source first: a source's own
    /// `BrainIndex::backlinks` already resolves same-source links correctly,
    /// so those come through unchanged (just qualified/stamped). Cross-source
    /// backlinks are raw targets in another catalogue that *don't* resolve to
    /// anything within that catalogue, but do match this entry.
    pub fn backlinks(&self, qualified_id: &str) -> Result<Vec<BrainEntry>> {
        let ctx = MergeContext::build(&self.sources)?;
        self.backlinks_in(&ctx, qualified_id)
    }

    fn backlinks_in(&self, ctx: &MergeContext, qualified_id: &str) -> Result<Vec<BrainEntry>> {
        let Some((source, raw_id)) = Self::split_qualified(qualified_id) else { return Ok(Vec::new()) };
        let Some(home) = self.sources.iter().find(|s| s.name == source) else { return Ok(Vec::new()) };
        let Some(target_entry) = home.index.get(raw_id)? else { return Ok(Vec::new()) };

        let mut seen: HashSet<String> = HashSet::new();
        let mut results: Vec<BrainEntry> = Vec::new();
        for e in home.index.backlinks(raw_id)? {
            let stamped = Self::stamp(source, e);
            if seen.insert(stamped.id.clone()) {
                results.push(stamped);
            }
        }

        for other in &self.sources {
            if other.name == source {
                continue;
            }
            let Some(other_entries) = ctx.entries.get(&other.name) else { continue };
            let Some(other_raw_links) = ctx.raw_links.get(&other.name) else { continue };
            for (from_id, target) in other_raw_links {
                if Self::resolves_within(other_entries, target) {
                    continue;
                }
                if link_target_matches(target, &target_entry) {
                    if let Some(from_entry) = other_entries.iter().find(|e| &e.id == from_id) {
                        let stamped = Self::stamp(&other.name, from_entry.clone());
                        if seen.insert(stamped.id.clone()) {
                            results.push(stamped);
                        }
                    }
                }
            }
        }
        results.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(results)
    }

    /// Resolved targets of `qualified_id`'s own links, same-source first
    /// (mirrors `backlinks`).
    pub fn outlinks(&self, qualified_id: &str) -> Result<Vec<BrainEntry>> {
        let ctx = MergeContext::build(&self.sources)?;
        self.outlinks_in(&ctx, qualified_id)
    }

    fn outlinks_in(&self, ctx: &MergeContext, qualified_id: &str) -> Result<Vec<BrainEntry>> {
        let Some((source, raw_id)) = Self::split_qualified(qualified_id) else { return Ok(Vec::new()) };
        let Some(home) = self.sources.iter().find(|s| s.name == source) else { return Ok(Vec::new()) };

        let mut seen: HashSet<String> = HashSet::new();
        let mut results: Vec<BrainEntry> = Vec::new();
        for e in home.index.outlinks(raw_id)? {
            let stamped = Self::stamp(source, e);
            if seen.insert(stamped.id.clone()) {
                results.push(stamped);
            }
        }

        if let (Some(home_entries), Some(home_raw_links)) =
            (ctx.entries.get(source), ctx.raw_links.get(source))
        {
            for (from_id, target) in home_raw_links {
                if from_id != raw_id || Self::resolves_within(home_entries, target) {
                    continue;
                }
                if let Some((other_name, hit)) = self.resolve_cross_source(ctx, source, target) {
                    let stamped = Self::stamp(&other_name, hit);
                    if seen.insert(stamped.id.clone()) {
                        results.push(stamped);
                    }
                }
            }
        }
        results.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(results)
    }

    /// The full resolved (from_id, to_id) edge list across every source, for
    /// graph rendering. Same-source-first per edge, exactly like
    /// `backlinks`/`outlinks`.
    pub fn links_all(&self) -> Result<Vec<(String, String)>> {
        let ctx = MergeContext::build(&self.sources)?;

        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut edges: Vec<(String, String)> = Vec::new();
        for src in &self.sources {
            for (from_id, to_id) in src.index.links_all()? {
                let edge = (Self::qualify(&src.name, &from_id), Self::qualify(&src.name, &to_id));
                if seen.insert(edge.clone()) {
                    edges.push(edge);
                }
            }

            let Some(home_entries) = ctx.entries.get(&src.name) else { continue };
            let Some(home_raw_links) = ctx.raw_links.get(&src.name) else { continue };
            for (from_id, target) in home_raw_links {
                if Self::resolves_within(home_entries, target) {
                    continue;
                }
                if let Some((other_name, hit)) = self.resolve_cross_source(&ctx, &src.name, target) {
                    let edge = (Self::qualify(&src.name, from_id), Self::qualify(&other_name, &hit.id));
                    if seen.insert(edge.clone()) {
                        edges.push(edge);
                    }
                }
            }
        }
        edges.sort();
        Ok(edges)
    }

    /// Entries related to `qualified_id`, same tiering as
    /// `BrainIndex::related` (direct links, then co-citation, then shared
    /// tags) but built on the cross-source-aware `backlinks`/`outlinks`
    /// above, so tiers 1-2 already span every source; tier 3 scans every
    /// source's entries for a shared tag. Builds one `MergeContext` up front
    /// and threads it through every nested lookup -- co-citation alone calls
    /// `backlinks`/`outlinks` once per link the entry has, so without a
    /// shared cache this re-scanned every other source's full entry table
    /// from scratch per link (found in review).
    pub fn related(&self, qualified_id: &str, limit: usize) -> Result<Vec<BrainEntry>> {
        let Some(entry) = self.get(qualified_id)? else { return Ok(Vec::new()) };
        let ctx = MergeContext::build(&self.sources)?;

        let mut ranked: Vec<BrainEntry> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(entry.id.clone());

        let outs = self.outlinks_in(&ctx, &entry.id)?;
        let backs = self.backlinks_in(&ctx, &entry.id)?;
        merge_unique(outs.clone(), &mut ranked, &mut seen);
        merge_unique(backs.clone(), &mut ranked, &mut seen);

        let mut co_citation: Vec<BrainEntry> = Vec::new();
        for target in &outs {
            co_citation.extend(self.backlinks_in(&ctx, &target.id)?);
        }
        for source in &backs {
            co_citation.extend(self.outlinks_in(&ctx, &source.id)?);
        }
        merge_unique(co_citation, &mut ranked, &mut seen);

        if !entry.tags.is_empty() {
            let mut tag_matches: Vec<BrainEntry> = Vec::new();
            for src in &self.sources {
                let Some(entries) = ctx.entries.get(&src.name) else { continue };
                for e in entries {
                    if entry.tags.iter().any(|t| e.tags.contains(t)) {
                        let stamped = Self::stamp(&src.name, e.clone());
                        if stamped.id != entry.id {
                            tag_matches.push(stamped);
                        }
                    }
                }
            }
            tag_matches.sort_by(|a, b| a.name.cmp(&b.name));
            merge_unique(tag_matches, &mut ranked, &mut seen);
        }

        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Reindexes every source, returning the summed stats.
    pub fn rebuild_all(&self, embedder: Option<&dyn Embedder>) -> Result<RebuildStats> {
        let mut total = RebuildStats { indexed: 0, embedded: 0, cached: 0 };
        for src in &self.sources {
            let stats = src.index.rebuild(embedder)?;
            total.indexed += stats.indexed;
            total.embedded += stats.embedded;
            total.cached += stats.cached;
        }
        Ok(total)
    }
}

/// What a Brain view is currently backed by: one catalogue's own index, or
/// the union of every enabled catalogue ("All sources"). Delegating methods
/// mean callers across both the GUI and the TUI don't need to know which;
/// only the code that switches catalogues and the freshen/reindex race
/// guards (via `identity`) care.
#[derive(Debug, Clone)]
pub enum BrainScope {
    Single(std::sync::Arc<BrainIndex>),
    All(std::sync::Arc<MergedBrainIndex>),
}

/// Identifies *what* a `BrainScope` is open on, for race guards comparing
/// an in-flight async result against the scope that's current by the time
/// it lands — replaces a bare `BrainIndex::path()` comparison, which has no
/// equivalent for `All` (there's no single path to compare). `All`'s sorted
/// catalogue-path list matters, not just the bare variant: two "All
/// sources" scopes covering different catalogue sets (one enabled/disabled
/// mid-flight) must compare unequal, or a stale result for the old set
/// could silently overwrite a newer result for the new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrainScopeId {
    Single(std::path::PathBuf),
    All(Vec<std::path::PathBuf>),
}

impl BrainScope {
    pub fn identity(&self) -> BrainScopeId {
        match self {
            BrainScope::Single(b) => BrainScopeId::Single(b.path().to_path_buf()),
            BrainScope::All(m) => {
                let mut paths: Vec<std::path::PathBuf> = m.sources().iter().map(|s| s.index.path().to_path_buf()).collect();
                paths.sort();
                BrainScopeId::All(paths)
            }
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self, BrainScope::All(_))
    }

    /// Does this scope cover the catalogue rooted at `path`? Single: the
    /// obvious direct comparison. All: true if any merged source is rooted
    /// there.
    pub fn includes(&self, path: &std::path::Path) -> bool {
        match self {
            BrainScope::Single(b) => b.path() == path,
            BrainScope::All(m) => m.sources().iter().any(|s| s.index.path() == path),
        }
    }

    /// The on-disk file an entry id resolves to. Single: joined directly
    /// onto the catalogue root. All: the id is qualified (`"source:id"`),
    /// so the source's own root is looked up first.
    pub fn file_path(&self, entry_id: &str) -> std::path::PathBuf {
        match self {
            BrainScope::Single(b) => b.path().join(entry_id),
            BrainScope::All(m) => match MergedBrainIndex::split_qualified(entry_id) {
                Some((source, raw_id)) => match m.sources().iter().find(|s| s.name == source) {
                    Some(src) => src.index.path().join(raw_id),
                    None => std::path::PathBuf::from(entry_id),
                },
                None => std::path::PathBuf::from(entry_id),
            },
        }
    }

    pub fn query(&self, text: &str, embedder: Option<&dyn Embedder>, filters: QueryFilters) -> Result<Vec<BrainEntry>> {
        match self {
            BrainScope::Single(b) => b.query(text, embedder, filters),
            BrainScope::All(m) => m.query(text, embedder, filters),
        }
    }

    pub fn backlinks(&self, id: &str) -> Result<Vec<BrainEntry>> {
        match self {
            BrainScope::Single(b) => b.backlinks(id),
            BrainScope::All(m) => m.backlinks(id),
        }
    }

    pub fn outlinks(&self, id: &str) -> Result<Vec<BrainEntry>> {
        match self {
            BrainScope::Single(b) => b.outlinks(id),
            BrainScope::All(m) => m.outlinks(id),
        }
    }

    pub fn related(&self, id: &str, limit: usize) -> Result<Vec<BrainEntry>> {
        match self {
            BrainScope::Single(b) => b.related(id, limit),
            BrainScope::All(m) => m.related(id, limit),
        }
    }

    pub fn links_all(&self) -> Result<Vec<(String, String)>> {
        match self {
            BrainScope::Single(b) => b.links_all(),
            BrainScope::All(m) => m.links_all(),
        }
    }

    pub fn rebuild(&self, embedder: Option<&dyn Embedder>) -> Result<RebuildStats> {
        match self {
            BrainScope::Single(b) => b.rebuild(embedder),
            BrainScope::All(m) => m.rebuild_all(embedder),
        }
    }

    /// Rebuilds just the one source rooted at `path`, not the whole scope.
    /// A no-op if `path` isn't part of this scope.
    pub fn rebuild_source(&self, path: &std::path::Path, embedder: Option<&dyn Embedder>) -> Result<()> {
        match self {
            BrainScope::Single(b) => {
                if b.path() == path {
                    b.rebuild(embedder)?;
                }
            }
            BrainScope::All(m) => {
                if let Some(src) = m.sources().iter().find(|s| s.index.path() == path) {
                    src.index.rebuild(embedder)?;
                }
            }
        }
        Ok(())
    }
}

/// The catalogue root a write for `id` should land in, resolved purely from
/// `options` (`AppConfig::catalogue_options()`) — no index needs to be open.
/// `id` qualified as `"source:id"` (an existing "All sources" entry)
/// resolves to that source's own root; an unqualified `id` (a brand-new
/// entry, which has no source yet) falls back to `default` — there's no
/// single natural catalogue for a new entry while "All sources" is active.
pub fn catalogue_root_for_id(id: &str, options: &[crate::config::CatalogueRef], default: &std::path::Path) -> std::path::PathBuf {
    match MergedBrainIndex::split_qualified(id) {
        Some((source, _)) => options.iter().find(|c| c.name == source).map(|c| c.path.clone()).unwrap_or_else(|| default.to_path_buf()),
        None => default.to_path_buf(),
    }
}

#[cfg(test)]
mod merged_tests {
    use super::*;
    use crate::config::CatalogueRef;
    use tempfile::tempdir;

    fn catalogue(name: &str, dir: &tempfile::TempDir, enabled: bool) -> CatalogueRef {
        CatalogueRef {
            name: name.to_string(),
            path: dir.path().to_path_buf(),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
            write_rules_override: None,
        }
    }

    fn write_entry(dir: &tempfile::TempDir, rel: &str, body: &str) {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn merge_stamps_source_and_qualifies_ids() {
        let a = tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget\n\nFrom A.");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget\n\nFrom B.");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let entries = merged.query("", None, QueryFilters::default()).unwrap();

        assert_eq!(entries.len(), 2);
        let widget = entries.iter().find(|e| e.name == "widget").unwrap();
        assert_eq!(widget.source, "alpha");
        assert_eq!(widget.id, "alpha:concepts/widget.md");
        let gadget = entries.iter().find(|e| e.name == "gadget").unwrap();
        assert_eq!(gadget.source, "beta");
        assert_eq!(gadget.id, "beta:concepts/gadget.md");
    }

    #[test]
    fn disabled_catalogue_is_excluded() {
        let a = tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget\n\nFrom A.");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget\n\nFrom B.");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, false)]).unwrap();

        assert_eq!(merged.sources().len(), 1);
        let entries = merged.query("", None, QueryFilters::default()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "alpha");
    }

    #[test]
    fn get_resolves_by_qualified_id_and_distinguishes_colliding_paths() {
        let a = tempdir().unwrap();
        write_entry(&a, "notes/same.md", "# Same\n\nFrom A.");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "notes/same.md", "# Same\n\nFrom B.");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();

        let from_a = merged.get("alpha:notes/same.md").unwrap().unwrap();
        assert_eq!(from_a.source, "alpha");
        assert!(from_a.body.contains("From A"));

        let from_b = merged.get("beta:notes/same.md").unwrap().unwrap();
        assert_eq!(from_b.source, "beta");
        assert!(from_b.body.contains("From B"));
    }

    #[test]
    fn query_text_merges_ranked_results_across_sources() {
        let a = tempdir().unwrap();
        write_entry(&a, "notes/one.md", "# One\n\nmentions crabapple here.");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "notes/two.md", "# Two\n\nalso mentions crabapple.");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let results = merged.query("crabapple", None, QueryFilters::default()).unwrap();

        assert_eq!(results.len(), 2);
        let sources: HashSet<&str> = results.iter().map(|e| e.source.as_str()).collect();
        assert!(sources.contains("alpha"));
        assert!(sources.contains("beta"));
    }

    #[test]
    fn wikilinks_resolve_within_the_same_source_before_crossing() {
        // Both catalogues have an entry named "Shared"; alpha's "home.md"
        // links to it and must resolve to alpha's own "Shared", not beta's.
        let a = tempdir().unwrap();
        write_entry(&a, "notes/home.md", "# Home\n\nSee [[Shared]].");
        write_entry(&a, "notes/shared.md", "---\nname: Shared\n---\n# Shared (alpha)");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "notes/shared.md", "---\nname: Shared\n---\n# Shared (beta)");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let outs = merged.outlinks("alpha:notes/home.md").unwrap();

        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].source, "alpha");
        assert!(outs[0].body.contains("alpha"));
    }

    #[test]
    fn wikilinks_fall_back_across_sources_when_unresolved_at_home() {
        // alpha's "home.md" links to "Shared", which only exists in beta.
        let a = tempdir().unwrap();
        write_entry(&a, "notes/home.md", "# Home\n\nSee [[Shared]].");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "notes/shared.md", "---\nname: Shared\n---\n# Shared (beta)");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let outs = merged.outlinks("alpha:notes/home.md").unwrap();

        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].source, "beta");

        let backs = merged.backlinks("beta:notes/shared.md").unwrap();
        assert_eq!(backs.len(), 1);
        assert_eq!(backs[0].id, "alpha:notes/home.md");
    }

    #[test]
    fn related_spans_sources_through_merged_backlinks_and_outlinks() {
        let a = tempdir().unwrap();
        write_entry(&a, "notes/home.md", "# Home\n\nSee [[Shared]].");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "notes/shared.md", "---\nname: Shared\n---\n# Shared (beta)");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let related = merged.related("alpha:notes/home.md", 10).unwrap();

        assert_eq!(related.len(), 1);
        assert_eq!(related[0].id, "beta:notes/shared.md");
    }

    #[test]
    fn links_all_includes_cross_source_edges() {
        let a = tempdir().unwrap();
        write_entry(&a, "notes/home.md", "# Home\n\nSee [[Shared]].");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        let b = tempdir().unwrap();
        write_entry(&b, "notes/shared.md", "---\nname: Shared\n---\n# Shared (beta)");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let edges = merged.links_all().unwrap();

        assert_eq!(edges, vec![("alpha:notes/home.md".to_string(), "beta:notes/shared.md".to_string())]);
    }

    #[test]
    fn a_catalogue_that_fails_to_open_is_skipped_not_fatal() {
        let a = tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();

        // A file where a directory is expected makes `BrainIndex::open` fail.
        let broken = tempdir().unwrap();
        let broken_path = broken.path().join("not-a-dir");
        fs::write(&broken_path, "oops").unwrap();
        let mut bad = catalogue("broken", &broken, true);
        bad.path = broken_path;

        let merged = MergedBrainIndex::open(&[catalogue("alpha", &a, true), bad]).unwrap();
        assert_eq!(merged.sources().len(), 1);
        assert_eq!(merged.sources()[0].name, "alpha");
    }

    #[test]
    fn rebuild_all_sums_stats_across_sources() {
        let a = tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget");
        let b = tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget");
        write_entry(&b, "concepts/gizmo.md", "# Gizmo");

        let merged =
            MergedBrainIndex::open(&[catalogue("alpha", &a, true), catalogue("beta", &b, true)]).unwrap();
        let stats = merged.rebuild_all(None).unwrap();
        assert_eq!(stats.indexed, 3);
    }
}

#[cfg(test)]
mod brain_scope_tests {
    use super::*;
    use crate::config::CatalogueRef;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn catalogue(name: &str, dir: &tempfile::TempDir) -> CatalogueRef {
        CatalogueRef {
            name: name.to_string(),
            path: dir.path().to_path_buf(),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
            write_rules_override: None,
        }
    }

    fn write_entry(dir: &tempfile::TempDir, rel: &str, body: &str) {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn single_scope_identity_and_includes_use_its_own_path() {
        let dir = tempdir().unwrap();
        write_entry(&dir, "concepts/widget.md", "# Widget");
        let index = BrainIndex::open(dir.path()).unwrap();
        index.rebuild(None).unwrap();
        let scope = BrainScope::Single(Arc::new(index));

        assert_eq!(scope.identity(), BrainScopeId::Single(dir.path().to_path_buf()));
        assert!(!scope.is_all());
        assert!(scope.includes(dir.path()));
        assert!(!scope.includes(std::path::Path::new("/nowhere")));
        assert_eq!(scope.file_path("concepts/widget.md"), dir.path().join("concepts/widget.md"));
    }

    #[test]
    fn all_scope_identity_is_the_sorted_source_paths() {
        let a = tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();
        let b = tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged = MergedBrainIndex::open(&[catalogue("alpha", &a), catalogue("beta", &b)]).unwrap();
        let scope = BrainScope::All(Arc::new(merged));

        assert!(scope.is_all());
        let mut expected = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        expected.sort();
        assert_eq!(scope.identity(), BrainScopeId::All(expected));
        assert!(scope.includes(a.path()));
        assert!(scope.includes(b.path()));
        assert!(!scope.includes(std::path::Path::new("/nowhere")));
    }

    #[test]
    fn all_scope_file_path_resolves_a_qualified_id_to_its_own_source_root() {
        let a = tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget");
        BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();
        let b = tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget");
        BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let merged = MergedBrainIndex::open(&[catalogue("alpha", &a), catalogue("beta", &b)]).unwrap();
        let scope = BrainScope::All(Arc::new(merged));

        assert_eq!(scope.file_path("beta:concepts/gadget.md"), b.path().join("concepts/gadget.md"));
    }

    #[test]
    fn catalogue_root_for_id_resolves_a_qualified_entry_to_its_own_catalogue() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let options = [catalogue("alpha", &a), catalogue("beta", &b)];
        let fallback = a.path();
        assert_eq!(catalogue_root_for_id("beta:concepts/gadget.md", &options, fallback), b.path());
    }

    #[test]
    fn catalogue_root_for_id_falls_back_to_default_for_an_unqualified_new_entry_id() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let options = [catalogue("alpha", &a), catalogue("beta", &b)];
        let fallback = a.path();
        assert_eq!(catalogue_root_for_id("concepts/brand-new.md", &options, fallback), fallback);
    }

    #[test]
    fn catalogue_root_for_id_falls_back_to_default_for_an_unknown_source() {
        let a = tempdir().unwrap();
        let options = [catalogue("alpha", &a)];
        let fallback = a.path();
        assert_eq!(catalogue_root_for_id("ghost:concepts/gone.md", &options, fallback), fallback);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fetch a single entry by id against an already-locked connection.
fn get_by_id(conn: &Connection, id: &str) -> Result<Option<BrainEntry>> {
    let mut stmt = conn
        .prepare("SELECT id, type, name, tags, repos, updated, body FROM entries WHERE id = ?1")?;
    let mut rows = stmt.query_map([id], row_to_entry)?;
    match rows.next() {
        None => Ok(None),
        Some(r) => Ok(Some(r?)),
    }
}

/// The file-stem of a path-like string: the last `/`-separated segment with
/// its trailing extension stripped. Used both from Rust and registered as a
/// SQL scalar function (`stem(x)`) so link-resolution queries can apply the
/// same rule the app uses when it needs to compare raw wikilink targets.
fn stem_of(s: &str) -> String {
    let base = s.rsplit('/').next().unwrap_or(s);
    match base.rsplit_once('.') {
        Some((stem, _ext)) if !stem.is_empty() => stem.to_string(),
        _ => base.to_string(),
    }
}

/// Extract raw wikilink targets from a markdown body, Obsidian-style.
///
/// Handles `[[target]]`, `[[target|alias]]` (target `target`),
/// `[[target#heading]]` / `[[target#^block]]` (target `target`), and
/// `[[target#heading|alias]]` (target `target`). Embeds (`![[x]]`) are
/// skipped. Targets are returned raw, exactly as written -- resolving them
/// to entry ids happens at query time via [`BrainIndex::backlinks`] /
/// [`BrainIndex::outlinks`] / [`BrainIndex::links_all`].
fn extract_wikilinks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel_start) = text[search_from..].find("[[") {
        let start = search_from + rel_start;
        let is_embed = start > 0 && text.as_bytes()[start - 1] == b'!';
        let inner_start = start + 2;
        let Some(rel_end) = text[inner_start..].find("]]") else {
            break;
        };
        let end = inner_start + rel_end;
        let inner = &text[inner_start..end];
        search_from = end + 2;

        if is_embed || inner.is_empty() {
            continue;
        }
        let before_alias = inner.split('|').next().unwrap_or(inner);
        let target = before_alias.split('#').next().unwrap_or(before_alias).trim();
        if !target.is_empty() {
            out.push(target.to_string());
        }
    }
    out
}

/// Everything derived from a single markdown file, computed off the main
/// thread during [`BrainIndex::rebuild`]. Pure data -- no connection, no I/O
/// side effects beyond the initial read -- so it's safely `Send`.
struct FileRecord {
    id: String,
    entry_type: String,
    name: String,
    tags: Vec<String>,
    repos: Vec<String>,
    updated: Option<String>,
    body: String,
    links: Vec<String>,
}

/// Read and parse a single candidate file into a [`FileRecord`]. Pure
/// function of its inputs (aside from the filesystem read), safe to call
/// from any thread in the rebuild worker pool.
fn process_file(brain_path: &Path, path: &Path) -> Option<FileRecord> {
    let rel = path
        .strip_prefix(brain_path)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let content = fs::read_to_string(path).ok()?;
    let parsed = parse_markdown(&content);

    // Without a frontmatter `type`, the kind is the top-level directory
    // under the brain (`repos/ninox/x.md` is a repo entry, not a "ninox"
    // one); a file at the brain root has none.
    let parent_type = path
        .strip_prefix(brain_path)
        .ok()
        .and_then(|rel| {
            let mut parts = rel.components();
            let first = parts.next()?;
            parts.next().map(|_| first)
        })
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_string);

    let entry_type = parsed
        .frontmatter
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(parent_type)
        .unwrap_or_else(|| "note".to_string());

    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    let name = parsed
        .frontmatter
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| stem.to_string());

    let tags: Vec<String> = parsed
        .frontmatter
        .get("tags")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    let repos: Vec<String> = parsed
        .frontmatter
        .get("repos")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    let updated = parsed
        .frontmatter
        .get("updated")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let links = extract_wikilinks(&parsed.body);

    Some(FileRecord { id: rel, entry_type, name, tags, repos, updated, body: parsed.body, links })
}

/// Turn free-form user search text into an FTS5 query string that can't be
/// misinterpreted as query syntax. Raw text handed straight to `MATCH`
/// exposes FTS5's operators (`-` for NOT, `:` for column filters, unmatched
/// `"` for phrases, etc.), so e.g. searching for `ninox-server` or `foo:bar`
/// throws a SQL error instead of finding matches. Wrapping each
/// whitespace-separated token in `"..."` (doubling internal quotes) forces
/// every token to be matched literally.
fn sanitize_fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Text an entry is embedded from: name plus body, truncated to a bounded
/// length. Small embedding models have a limited token context; truncating
/// to a character prefix is a pragmatic approximation, not a guarantee the
/// whole document is represented (see the design spec's non-goals — full
/// chunking is out of scope).
fn embedding_text(rec: &FileRecord) -> String {
    let mut text = format!("{}\n\n{}", rec.name, rec.body);
    text.truncate(2000);
    text
}

/// Non-cryptographic hash used purely to detect content changes between
/// `rebuild()` calls. Not guaranteed stable across Rust/std versions — the
/// worst case of that is one redundant re-embed after an upgrade, never a
/// correctness problem, so no extra hashing dependency is justified.
fn content_hash(text: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish() as i64
}

fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vector(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn row_to_entry(r: &rusqlite::Row) -> rusqlite::Result<BrainEntry> {
    let tags_json: String = r.get(3)?;
    let repos_json: String = r.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let repos: Vec<String> = serde_json::from_str(&repos_json).unwrap_or_default();
    Ok(BrainEntry {
        id: r.get(0)?,
        entry_type: r.get(1)?,
        name: r.get(2)?,
        tags,
        repos,
        updated: r.get(5)?,
        body: r.get(6)?,
        source: String::new(),
    })
}

/// Ensure the brain's derived/local-only files are in its `.gitignore`:
/// the SQLite index plus the remote-sync marker and state files (see
/// `brain_sync`), none of which should ever be committed or synced.
fn ensure_gitignore(brain_path: &Path) -> Result<()> {
    let gi = brain_path.join(".gitignore");
    let wanted = [".index.db", ".sync.toml", ".sync-state.json"];
    let mut content = if gi.exists() { fs::read_to_string(&gi)? } else { String::new() };
    let mut changed = !gi.exists();
    for entry in wanted {
        if !content.lines().any(|l| l.trim() == entry) {
            content.push_str(entry);
            content.push('\n');
            changed = true;
        }
    }
    if changed {
        fs::write(&gi, content)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frontmatter parsing (manual YAML split, no extra dep)
// ---------------------------------------------------------------------------

/// A scalar frontmatter field (`name`, `type`, …) of a markdown entry, read
/// the way the indexer reads it; `None` when absent or empty.
pub fn frontmatter_str(content: &str, key: &str) -> Option<String> {
    parse_markdown(content).frontmatter.get(key).and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}

struct FmValue {
    str_val: Option<String>,
    seq_val: Option<Vec<String>>,
}

impl FmValue {
    fn str(s: &str) -> Self {
        Self { str_val: Some(s.to_string()), seq_val: None }
    }

    fn seq(v: Vec<String>) -> Self {
        Self { str_val: None, seq_val: Some(v) }
    }

    fn as_str(&self) -> Option<&str> {
        self.str_val.as_deref()
    }

    fn as_sequence(&self) -> Option<&Vec<String>> {
        self.seq_val.as_ref()
    }
}

struct Frontmatter(HashMap<String, FmValue>);

impl Frontmatter {
    fn get(&self, key: &str) -> Option<&FmValue> {
        self.0.get(key)
    }
}

struct ParsedMd {
    frontmatter: Frontmatter,
    body: String,
}

fn parse_markdown(content: &str) -> ParsedMd {
    if !content.starts_with("---") {
        return ParsedMd {
            frontmatter: Frontmatter(HashMap::new()),
            body: content.to_string(),
        };
    }
    let rest = &content[3..];
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"));
    let (fm_text, body) = match end {
        None => ("", content),
        Some(pos) => {
            let after = &rest[pos + 4..]; // skip "\n---"
            // skip optional trailing newline
            let body = after.trim_start_matches('\n').trim_start_matches('\r');
            (&rest[..pos], body)
        }
    };
    let fm = parse_frontmatter(fm_text);
    ParsedMd { frontmatter: fm, body: body.to_string() }
}

fn parse_frontmatter(text: &str) -> Frontmatter {
    let mut map: HashMap<String, FmValue> = HashMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim().to_string();
            let val = val.trim();
            if val.is_empty() {
                // Possibly a sequence starting on the next lines
                let mut seq = Vec::new();
                while let Some(next) = lines.peek() {
                    let t = next.trim();
                    if let Some(stripped) = t.strip_prefix("- ") {
                        seq.push(stripped.trim().to_string());
                        lines.next();
                    } else {
                        break;
                    }
                }
                if !seq.is_empty() {
                    map.insert(key, FmValue::seq(seq));
                }
            } else if val.starts_with('[') && val.ends_with(']') {
                // Inline sequence: [a, b, c]
                let inner = &val[1..val.len() - 1];
                let seq: Vec<String> = inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                map.insert(key, FmValue::seq(seq));
            } else {
                map.insert(
                    key,
                    FmValue::str(val.trim_matches('"').trim_matches('\'')),
                );
            }
        }
    }
    Frontmatter(map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::Embedder;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn make_brain() -> (BrainIndex, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let brain = BrainIndex::open(dir.path()).unwrap();
        (brain, dir)
    }

    #[test]
    fn frontmatter_str_reads_scalars_only() {
        let md = "---\nname: Tmux sockets\ntype: \"concepts\"\nempty:\ntags: [a, b]\n---\nbody";
        assert_eq!(frontmatter_str(md, "name").as_deref(), Some("Tmux sockets"));
        assert_eq!(frontmatter_str(md, "type").as_deref(), Some("concepts"));
        assert_eq!(frontmatter_str(md, "empty"), None);
        assert_eq!(frontmatter_str(md, "tags"), None);
        assert_eq!(frontmatter_str("no frontmatter", "name"), None);
    }

    /// Deterministic, call-counting fake — never touches the network or a
    /// real model. The vector is derived from the text's length so distinct
    /// inputs get distinct (but stable) vectors, which is enough for cache
    /// and fusion tests without needing real semantic meaning.
    struct FakeEmbedder {
        calls: AtomicUsize,
        dim: usize,
    }

    impl FakeEmbedder {
        fn new(dim: usize) -> Self {
            Self { calls: AtomicUsize::new(0), dim }
        }
    }

    impl Embedder for FakeEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let seed = text.len() as f32;
            Ok((0..self.dim).map(|i| seed + i as f32).collect())
        }

        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        fn dimension(&self) -> usize {
            self.dim
        }
    }

    #[test]
    fn rebuild_embeds_new_entries() {
        let (brain, dir) = make_brain();
        fs::create_dir_all(dir.path().join("notes")).unwrap();
        fs::write(dir.path().join("notes/a.md"), "# A\n\nSome content.").unwrap();

        let embedder = FakeEmbedder::new(4);
        let stats = brain.rebuild(Some(&embedder)).unwrap();

        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.embedded, 1);
        assert_eq!(stats.cached, 0);
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rebuild_reuses_cached_embedding_for_unchanged_content() {
        let (brain, dir) = make_brain();
        fs::create_dir_all(dir.path().join("notes")).unwrap();
        fs::write(dir.path().join("notes/a.md"), "# A\n\nSome content.").unwrap();

        let embedder = FakeEmbedder::new(4);
        brain.rebuild(Some(&embedder)).unwrap();
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);

        // Second rebuild, same content: must not re-embed.
        let stats = brain.rebuild(Some(&embedder)).unwrap();
        assert_eq!(stats.embedded, 0);
        assert_eq!(stats.cached, 1);
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1, "should not re-embed unchanged content");
    }

    #[test]
    fn rebuild_reembeds_changed_content() {
        let (brain, dir) = make_brain();
        let path = dir.path().join("notes");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("a.md"), "# A\n\nOriginal content.").unwrap();

        let embedder = FakeEmbedder::new(4);
        brain.rebuild(Some(&embedder)).unwrap();
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);

        fs::write(path.join("a.md"), "# A\n\nChanged content.").unwrap();
        let stats = brain.rebuild(Some(&embedder)).unwrap();
        assert_eq!(stats.embedded, 1);
        assert_eq!(stats.cached, 0);
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rebuild_prunes_embeddings_for_deleted_entries() {
        let (brain, dir) = make_brain();
        let path = dir.path().join("notes");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("a.md"), "# A\n\nContent.").unwrap();

        let embedder = FakeEmbedder::new(4);
        brain.rebuild(Some(&embedder)).unwrap();

        fs::remove_file(path.join("a.md")).unwrap();
        brain.rebuild(Some(&embedder)).unwrap();

        let conn = brain.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "embedding for the deleted entry should be pruned");
    }

    #[test]
    fn rebuild_without_embedder_skips_embeddings_entirely() {
        let (brain, dir) = make_brain();
        fs::create_dir_all(dir.path().join("notes")).unwrap();
        fs::write(dir.path().join("notes/a.md"), "# A\n\nContent.").unwrap();

        let stats = brain.rebuild(None).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.embedded, 0);
        assert_eq!(stats.cached, 0);

        let conn = brain.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }

    /// An embedder whose calls always fail — proves indexing is never
    /// blocked by embedding failures.
    struct FailingEmbedder;
    impl Embedder for FailingEmbedder {
        fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("simulated embedder failure")
        }
        fn embed_batch(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            anyhow::bail!("simulated embedder failure")
        }
        fn dimension(&self) -> usize {
            4
        }
    }

    #[test]
    fn rebuild_indexing_survives_embedder_failure() {
        let (brain, dir) = make_brain();
        fs::create_dir_all(dir.path().join("notes")).unwrap();
        fs::write(dir.path().join("notes/a.md"), "# A\n\nContent.").unwrap();

        let stats = brain.rebuild(Some(&FailingEmbedder)).unwrap();
        assert_eq!(stats.indexed, 1, "keyword indexing must succeed even if embedding fails");
        assert_eq!(stats.embedded, 0);
    }

    #[test]
    fn open_creates_schema() {
        let (_brain, dir) = make_brain();
        let db_path = dir.path().join(".index.db");
        assert!(db_path.exists());
        // Verify the gitignore was created
        let gi = dir.path().join(".gitignore");
        assert!(gi.exists());
        let content = fs::read_to_string(&gi).unwrap();
        assert!(content.contains(".index.db"));
    }

    #[test]
    fn gitignore_covers_sync_files() {
        let (_brain, dir) = make_brain();
        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".index.db"));
        assert!(content.contains(".sync.toml"));
        assert!(content.contains(".sync-state.json"));
    }

    #[test]
    fn open_creates_embeddings_table() {
        let (brain, _dir) = make_brain();
        let conn = brain.conn.lock().unwrap();
        // A query against the table succeeding at all (vs. an SQL error)
        // proves the table exists with these columns.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rebuild_indexes_files() {
        let (brain, dir) = make_brain();
        let people_dir = dir.path().join("people");
        fs::create_dir_all(&people_dir).unwrap();
        fs::write(
            people_dir.join("alice.md"),
            "---\nname: Alice Smith\ntags:\n- engineering\n- leadership\n---\nAlice leads the infra team.",
        )
        .unwrap();
        fs::write(
            people_dir.join("bob.md"),
            "# Bob\n\nBob works on frontend.",
        )
        .unwrap();

        let stats = brain.rebuild(None).unwrap();
        assert_eq!(stats.indexed, 2);
    }

    #[test]
    fn query_returns_matches() {
        let (brain, dir) = make_brain();
        let dir_path = dir.path().join("notes");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(
            dir_path.join("rust-tips.md"),
            "---\nname: Rust Tips\ntags:\n- rust\n---\nUse anyhow for error handling.",
        )
        .unwrap();
        fs::write(
            dir_path.join("python-tips.md"),
            "---\nname: Python Tips\ntags:\n- python\n---\nUse dataclasses for data.",
        )
        .unwrap();

        brain.rebuild(None).unwrap();

        let results = brain.query("anyhow", None, QueryFilters::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Rust Tips");
    }

    /// FTS5 treats `-`, `:`, and unmatched `"` as query-syntax operators.
    /// Raw user search text containing them (e.g. a hyphenated crate name)
    /// must not surface as a SQL error.
    #[test]
    fn query_tolerates_special_characters() {
        let (brain, dir) = make_brain();
        let dir_path = dir.path().join("repos");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(
            dir_path.join("ninox-server.md"),
            "---\nname: ninox-server\n---\nHTTP API for ninox-server.",
        )
        .unwrap();

        brain.rebuild(None).unwrap();

        for text in ["ninox-server", "foo:bar", "orchestrator\"", "-orchestrator"] {
            brain
                .query(text, None, QueryFilters::default())
                .unwrap_or_else(|e| panic!("query({text:?}) should not error, got: {e}"));
        }

        let results = brain.query("ninox-server", None, QueryFilters::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "ninox-server");
    }

    #[test]
    fn query_filters_by_type() {
        let (brain, dir) = make_brain();
        let people = dir.path().join("people");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&people).unwrap();
        fs::create_dir_all(&projects).unwrap();
        fs::write(people.join("alice.md"), "Alice is a person.").unwrap();
        fs::write(projects.join("athene.md"), "Athene is a project.").unwrap();

        brain.rebuild(None).unwrap();

        let results = brain
            .query("", None, QueryFilters { entry_type: Some("people".into()), tag: None })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_type, "people");
    }

    #[test]
    fn query_surfaces_semantic_match_with_no_keyword_overlap() {
        let (brain, dir) = make_brain();
        let notes = dir.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        // Deliberately no shared words with the query text below.
        fs::write(notes.join("outage.md"), "---\nname: 401 debugging notes\n---\nHow we diagnosed the outage.").unwrap();
        fs::write(notes.join("coffee.md"), "---\nname: Coffee notes\n---\nBest beans for espresso.").unwrap();

        // A fake embedder that makes "auth failures" and "401 debugging
        // notes" cosine-similar (same first dimension marker) while
        // "Coffee notes" is dissimilar — enough to prove the fusion path
        // runs end-to-end without needing the real model.
        struct SemanticFakeEmbedder;
        impl Embedder for SemanticFakeEmbedder {
            fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
                if text.contains("auth") || text.contains("401") || text.contains("outage") {
                    Ok(vec![1.0, 0.0])
                } else {
                    Ok(vec![0.0, 1.0])
                }
            }
            fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
                texts.iter().map(|t| self.embed(t)).collect()
            }
            fn dimension(&self) -> usize {
                2
            }
        }

        let embedder = SemanticFakeEmbedder;
        brain.rebuild(Some(&embedder)).unwrap();

        // No shared vocabulary with either file's title/body at all, so the
        // keyword leg alone would return nothing for this query.
        let results = brain.query("auth failures", Some(&embedder), QueryFilters::default()).unwrap();
        assert!(
            results.iter().any(|e| e.id == "notes/outage.md"),
            "expected the semantically related entry to surface: {results:?}"
        );
        // The design applies no similarity threshold (see the spec's
        // "Query flow" section) — a dissimilar entry may still appear, it
        // must just rank below the related one if both are present.
        let outage_rank = results.iter().position(|e| e.id == "notes/outage.md").unwrap();
        if let Some(coffee_rank) = results.iter().position(|e| e.id == "notes/coffee.md") {
            assert!(
                outage_rank < coffee_rank,
                "the semantically related entry should outrank the unrelated one: {results:?}"
            );
        }
    }

    #[test]
    fn query_degrades_gracefully_when_embedder_fails_at_query_time() {
        let (brain, dir) = make_brain();
        let notes = dir.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("rust.md"), "---\nname: Rust Tips\n---\nUse anyhow for error handling.").unwrap();

        // Indexed without embeddings (embedder is only used at query time
        // here), then queried with an embedder whose every call fails.
        brain.rebuild(None).unwrap();
        let results = brain.query("anyhow", Some(&FailingEmbedder), QueryFilters::default()).unwrap();

        assert_eq!(
            results.len(),
            1,
            "a failing embedder must degrade to keyword-only results, not fail the whole query"
        );
        assert_eq!(results[0].name, "Rust Tips");
    }

    #[test]
    fn query_without_embedder_matches_current_keyword_only_behavior() {
        let (brain, dir) = make_brain();
        let notes = dir.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("rust.md"), "---\nname: Rust Tips\n---\nUse anyhow for error handling.").unwrap();
        brain.rebuild(None).unwrap();

        let results = brain.query("anyhow", None, QueryFilters::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Rust Tips");
    }

    #[test]
    fn query_empty_text_ignores_embedder() {
        let (brain, dir) = make_brain();
        fs::create_dir_all(dir.path().join("notes")).unwrap();
        fs::write(dir.path().join("notes/a.md"), "# A\n\nContent.").unwrap();
        let embedder = FakeEmbedder::new(4);
        brain.rebuild(Some(&embedder)).unwrap();
        embedder.calls.store(0, Ordering::SeqCst);

        let results = brain.query("", Some(&embedder), QueryFilters::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            0,
            "empty-text queries must never touch the embedder"
        );
    }

    /// Write `n` markdown files, each containing the shared keyword
    /// "widgetronic" so a keyword search for it matches all of them.
    fn write_many_matching_notes(dir: &Path, n: usize) {
        let notes = dir.join("notes");
        fs::create_dir_all(&notes).unwrap();
        for i in 0..n {
            fs::write(
                notes.join(format!("note{i}.md")),
                format!("---\nname: Note {i}\n---\nAll about widgetronic devices, entry {i}."),
            )
            .unwrap();
        }
    }

    #[test]
    fn query_without_embedder_returns_all_keyword_matches_uncapped() {
        let (brain, dir) = make_brain();
        write_many_matching_notes(dir.path(), 25);
        brain.rebuild(None).unwrap();

        let results = brain.query("widgetronic", None, QueryFilters::default()).unwrap();
        assert_eq!(
            results.len(),
            25,
            "the no-embedder keyword path must stay exactly today's uncapped behavior, not truncate to 20: {}",
            results.len()
        );
    }

    #[test]
    fn query_hybrid_fusion_caps_at_20() {
        let (brain, dir) = make_brain();
        write_many_matching_notes(dir.path(), 25);

        // FakeEmbedder succeeds for every input, so all 25 entries get
        // embeddings and `semantic_candidates` always returns a non-empty
        // (top-20) list — enough to force the fused path to actually run.
        let embedder = FakeEmbedder::new(4);
        brain.rebuild(Some(&embedder)).unwrap();

        let results = brain.query("widgetronic", Some(&embedder), QueryFilters::default()).unwrap();
        assert_eq!(
            results.len(),
            20,
            "the fused (hybrid) path must still cap at 20 results"
        );
    }

    // -----------------------------------------------------------------
    // Wikilink extraction
    // -----------------------------------------------------------------

    #[test]
    fn wikilink_plain() {
        assert_eq!(extract_wikilinks("see [[Target]] please"), vec!["Target"]);
    }

    #[test]
    fn wikilink_with_alias() {
        assert_eq!(extract_wikilinks("see [[Target|shown text]]"), vec!["Target"]);
    }

    #[test]
    fn wikilink_with_heading() {
        assert_eq!(extract_wikilinks("see [[Target#Heading]]"), vec!["Target"]);
    }

    #[test]
    fn wikilink_with_block_ref() {
        assert_eq!(extract_wikilinks("see [[Target#^abc123]]"), vec!["Target"]);
    }

    #[test]
    fn wikilink_with_heading_and_alias() {
        assert_eq!(extract_wikilinks("see [[a#b|c]]"), vec!["a"]);
    }

    #[test]
    fn wikilink_skips_embeds() {
        assert_eq!(extract_wikilinks("![[embedded-image]]"), Vec::<String>::new());
    }

    #[test]
    fn wikilink_multiple_and_mixed() {
        let text = "Links: [[one]], ![[skip-me]], [[two|Two]], and [[three#Sec|Three]].";
        assert_eq!(extract_wikilinks(text), vec!["one", "two", "three"]);
    }

    #[test]
    fn stem_of_strips_path_and_extension() {
        assert_eq!(stem_of("people/alice.md"), "alice");
        assert_eq!(stem_of("alice"), "alice");
        assert_eq!(stem_of("alice.md"), "alice");
        assert_eq!(stem_of("a/b/c.md"), "c");
    }

    // -----------------------------------------------------------------
    // Link-graph fixture for backlinks / outlinks / links_all / related
    // -----------------------------------------------------------------

    /// Builds a small vault with a known link graph:
    ///   alice --[[bob]]--> bob            (stem match: "bob" == stem("people/bob.md"))
    ///   alice --[[projects/athene.md|Athene]]--> athene   (exact id match)
    ///   bob   --[[alice]]--> alice        (stem match)
    ///   carol --[[bob]]--> bob            (stem match; co-cites bob with alice)
    ///   dave: no links, shares the "infra" tag with alice only.
    fn make_linked_brain() -> (BrainIndex, tempfile::TempDir) {
        let (brain, dir) = make_brain();
        let people = dir.path().join("people");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&people).unwrap();
        fs::create_dir_all(&projects).unwrap();

        fs::write(
            people.join("alice.md"),
            "---\nname: Alice\ntags:\n- infra\n- leads\n---\n\
             Manager of [[bob]] and works with [[projects/athene.md|Athene]]. \
             See [[bob#Contact]] too. Also embed ![[ignored]].",
        )
        .unwrap();
        fs::write(
            people.join("bob.md"),
            "---\nname: Bob\ntags:\n- infra\n---\nReports to [[alice]].",
        )
        .unwrap();
        fs::write(
            projects.join("athene.md"),
            "---\nname: Athene\ntags:\n- platform\n---\nFlagship project.",
        )
        .unwrap();
        fs::write(
            people.join("carol.md"),
            "---\nname: Carol\ntags:\n- ops\n---\nAlso reports to [[bob]].",
        )
        .unwrap();
        fs::write(
            people.join("dave.md"),
            "---\nname: Dave\ntags:\n- infra\n---\nNo links, just tag overlap.",
        )
        .unwrap();

        brain.rebuild(None).unwrap();
        (brain, dir)
    }

    #[test]
    fn outlinks_resolves_stem_and_id_matches() {
        let (brain, _dir) = make_linked_brain();
        let outs = brain.outlinks("people/alice.md").unwrap();
        let ids: Vec<&str> = outs.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"people/bob.md"), "expected stem-matched bob in {ids:?}");
        assert!(ids.contains(&"projects/athene.md"), "expected id-matched athene in {ids:?}");
        assert_eq!(outs.len(), 2, "duplicate [[bob]] mentions should be deduped: {ids:?}");
    }

    #[test]
    fn backlinks_resolves_incoming_links() {
        let (brain, _dir) = make_linked_brain();
        let backs = brain.backlinks("people/bob.md").unwrap();
        let ids: Vec<&str> = backs.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"people/alice.md"));
        assert!(ids.contains(&"people/carol.md"));
        assert_eq!(ids.len(), 2);

        let alice_backs = brain.backlinks("people/alice.md").unwrap();
        assert_eq!(alice_backs.len(), 1);
        assert_eq!(alice_backs[0].id, "people/bob.md");
    }

    #[test]
    fn backlinks_and_outlinks_empty_for_unknown_or_unlinked() {
        let (brain, _dir) = make_linked_brain();
        assert!(brain.backlinks("nowhere.md").unwrap().is_empty());
        assert!(brain.outlinks("people/dave.md").unwrap().is_empty());
    }

    #[test]
    fn links_all_returns_resolved_edges() {
        let (brain, _dir) = make_linked_brain();
        let edges = brain.links_all().unwrap();
        assert!(edges.contains(&("people/alice.md".to_string(), "people/bob.md".to_string())));
        assert!(edges.contains(&(
            "people/alice.md".to_string(),
            "projects/athene.md".to_string()
        )));
        assert!(edges.contains(&("people/bob.md".to_string(), "people/alice.md".to_string())));
        assert!(edges.contains(&("people/carol.md".to_string(), "people/bob.md".to_string())));
        assert_eq!(edges.len(), 4, "edges should be deduped: {edges:?}");
    }

    #[test]
    fn related_ranks_direct_links_then_co_citation_then_tags() {
        let (brain, _dir) = make_linked_brain();
        let related = brain.related("people/alice.md", 10).unwrap();
        let ids: Vec<&str> = related.iter().map(|e| e.id.as_str()).collect();

        // Never includes self.
        assert!(!ids.contains(&"people/alice.md"));

        let pos = |id: &str| ids.iter().position(|x| *x == id);
        let bob = pos("people/bob.md").expect("bob is a direct link");
        let athene = pos("projects/athene.md").expect("athene is a direct link");
        let carol = pos("people/carol.md").expect("carol co-cites bob with alice");
        let dave = pos("people/dave.md").expect("dave shares the infra tag");

        // Tier 1 (direct links) ranks above tier 2 (co-citation) which ranks
        // above tier 3 (shared tag only).
        assert!(bob < carol && athene < carol, "direct links should outrank co-citation");
        assert!(carol < dave, "co-citation should outrank shared-tag-only");
    }

    #[test]
    fn related_respects_limit() {
        let (brain, _dir) = make_linked_brain();
        let related = brain.related("people/alice.md", 1).unwrap();
        assert_eq!(related.len(), 1);
    }

    #[test]
    fn related_unknown_id_is_empty() {
        let (brain, _dir) = make_linked_brain();
        assert!(brain.related("nowhere.md", 10).unwrap().is_empty());
    }

    // -----------------------------------------------------------------
    // Scale proof
    // -----------------------------------------------------------------

    /// Synthesizes a vault with `n` markdown files spread across nested
    /// folders, each with realistic-ish frontmatter, body size, and a
    /// handful of wikilinks to other generated files.
    fn generate_synthetic_vault(root: &Path, n: usize) {
        let folders = ["people", "projects", "notes", "meetings", "areas"];
        // Pad body text out to roughly 2-4KB per file.
        let filler = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                       Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n";

        for i in 0..n {
            let folder = folders[i % folders.len()];
            let sub = i % 20; // nest further to exercise deeper directory walks
            let dir = root.join(folder).join(format!("sub{sub}"));
            fs::create_dir_all(&dir).unwrap();

            let mut body = String::new();
            for _ in 0..30 {
                body.push_str(filler);
            }
            // ~8 wikilinks to other files in the vault.
            for k in 0..8 {
                let target_i = (i + k * 37 + 1) % n;
                let target_folder = folders[target_i % folders.len()];
                body.push_str(&format!("See [[{target_folder}/note{target_i}]].\n"));
            }

            let content = format!(
                "---\nname: Note {i}\ntags:\n- tag{}\n- shared\nupdated: 2026-01-01\n---\n{body}",
                i % 50
            );
            fs::write(dir.join(format!("note{i}.md")), content).unwrap();
        }
    }

    #[test]
    fn rebuild_scales_to_500_files_within_ceiling() {
        let dir = tempdir().unwrap();
        generate_synthetic_vault(dir.path(), 500);
        let brain = BrainIndex::open(dir.path()).unwrap();

        let start = std::time::Instant::now();
        let stats = brain.rebuild(None).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(stats.indexed, 500);
        println!("rebuild of 500 files took {elapsed:?}");
        // Generous ceiling: this is here to catch a catastrophic regression
        // (e.g. an accidental fsync-per-file reintroduction), not to pin
        // down exact performance.
        assert!(elapsed.as_secs() < 10, "rebuild of 500 files took too long: {elapsed:?}");
    }

    #[test]
    #[ignore = "benchmark: run explicitly with `cargo test -p ninox-core --release -- --ignored rebuild_scales_to_5000_files -- --nocapture`"]
    fn rebuild_scales_to_5000_files() {
        let dir = tempdir().unwrap();
        generate_synthetic_vault(dir.path(), 5_000);
        let brain = BrainIndex::open(dir.path()).unwrap();

        let start = std::time::Instant::now();
        let stats = brain.rebuild(None).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(stats.indexed, 5_000);
        println!("rebuild of 5,000 files took {elapsed:?}");
    }
}
