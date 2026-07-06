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
}

#[derive(Debug, Default)]
pub struct QueryFilters {
    pub entry_type: Option<String>,
    pub tag: Option<String>,
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

impl BrainIndex {
    pub fn open(brain_path: impl AsRef<Path>) -> Result<Self> {
        let brain_path = brain_path.as_ref().to_path_buf();
        fs::create_dir_all(&brain_path)
            .with_context(|| format!("create brain dir {brain_path:?}"))?;
        let db_path = brain_path.join(".index.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open brain db {db_path:?}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
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
                    for ((rec, _, hash), vector) in to_embed.iter().zip(vectors.iter()) {
                        upsert.execute(params![rec.id, hash, vector_to_blob(vector)])?;
                    }
                    to_embed.len()
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

        results.truncate(20);
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
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
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

    // Derive entry_type from parent dir name (or frontmatter "type" field)
    let parent_type = path
        .parent()
        .and_then(|p| {
            // If the parent IS brain_path itself, there's no meaningful type dir
            if p == brain_path { None } else { p.file_name() }
        })
        .and_then(|n| n.to_str())
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
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
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
    })
}

/// Ensure `.index.db` is in the brain directory's `.gitignore`.
fn ensure_gitignore(brain_path: &Path) -> Result<()> {
    let gi = brain_path.join(".gitignore");
    let entry = ".index.db\n";
    if gi.exists() {
        let content = fs::read_to_string(&gi)?;
        if !content.contains(".index.db") {
            fs::write(&gi, format!("{content}{entry}"))?;
        }
    } else {
        fs::write(&gi, entry)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frontmatter parsing (manual YAML split, no extra dep)
// ---------------------------------------------------------------------------

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
