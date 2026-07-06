# Brain Hybrid Semantic Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ninox brain query <text>` finds conceptually related entries even when they share no vocabulary with the query, by blending the existing FTS5 keyword ranking with vector similarity from a local embedding model, as the default (non-flagged) behavior.

**Architecture:** A new `Embedder` trait in `ninox-core` abstracts embedding generation so tests never touch the real model. `FastEmbedEmbedder` implements it using the `fastembed` crate with the `snowflake-arctic-embed-xs` model. `BrainIndex::rebuild()` gains a caching embedding pass (new `embeddings` SQLite table, keyed by content hash so unchanged files never re-embed); `BrainIndex::query()` gains a semantic leg fused with the existing FTS5 leg via Reciprocal Rank Fusion. Both methods take `embedder: Option<&dyn Embedder>` as a parameter — `BrainIndex` never owns the embedder itself, which is what keeps construction lazy and testing trivial.

**Tech Stack:** Rust, `fastembed` v5 (ONNX Runtime via `ort`), `rusqlite` (already in use), `rayon` (already in use for `rebuild()`'s parallel file walk).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-06-brain-semantic-search-design.md` — this plan implements it exactly; consult it for the "why" behind any decision below.
- Model: `EmbeddingModel::SnowflakeArcticEmbedXS` (22M params, 384-dim). Do not substitute another model.
- Query text gets the instruction prefix `"Represent this sentence for searching relevant passages: "`; passage/entry text never does.
- No `--limit` CLI flag, no similarity threshold, no document chunking beyond a fixed truncation, no reranking model, no ANN/vector index. These are explicit non-goals in the spec — do not add them.
- `rebuild()` and `query()` degrade to exactly today's keyword-only behavior when passed `embedder: None`. This must hold for every code path touched.
- `cargo test --workspace` must never require network access or the real ONNX model. All new logic is tested through `Embedder`-trait fakes.

---

## Task 1: `Embedder` trait and cosine similarity

**Files:**
- Create: `crates/ninox-core/src/embeddings.rs`
- Modify: `crates/ninox-core/src/lib.rs`

**Interfaces:**
- Produces: `pub trait Embedder: Send + Sync { fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>; fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>; fn dimension(&self) -> usize; }`
- Produces: `pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32`

- [ ] **Step 1: Write the failing test for `cosine_similarity`**

Create `crates/ninox-core/src/embeddings.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical_vectors_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite_vectors_is_negative_one() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector_is_zero_not_nan() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 2.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p ninox-core embeddings:: 2>&1 | tail -20`
Expected: compile error, `cosine_similarity` and the module's public items don't exist yet.

- [ ] **Step 3: Implement `Embedder` trait and `cosine_similarity`**

Add above the test module in `crates/ninox-core/src/embeddings.rs`:

```rust
use anyhow::Result;

/// Turns text into a fixed-length vector for semantic similarity search.
/// Kept as a trait (rather than calling `fastembed` directly from
/// `brain.rs`) so every other test in the codebase can use a fake
/// implementation with no network access, no ONNX runtime, and
/// deterministic output.
pub trait Embedder: Send + Sync {
    /// Embed a single piece of text (e.g. a search query).
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Embed many pieces of text in one batched call (e.g. during
    /// `rebuild()`) — significantly faster than calling `embed` in a loop.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// The length of every vector this embedder produces.
    fn dimension(&self) -> usize;
}

/// Cosine similarity between two equal-length vectors, in `[-1.0, 1.0]`.
/// Returns `0.0` for a zero vector rather than dividing by zero / producing
/// `NaN` — a zero vector carries no directional information, so "no
/// similarity" is the correct answer.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}
```

- [ ] **Step 4: Register the module**

In `crates/ninox-core/src/lib.rs`, add `pub mod embeddings;` alphabetically among the existing `pub mod` lines (between `pub mod config;` and `pub mod events;`):

```rust
pub mod brain;
pub mod client;
pub mod config;
pub mod embeddings;
pub mod events;
pub mod github;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ninox-core embeddings:: 2>&1 | tail -20`
Expected: `4 passed; 0 failed`

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/embeddings.rs crates/ninox-core/src/lib.rs
git commit -m "feat(brain): add Embedder trait and cosine_similarity"
```

---

## Task 2: Reciprocal Rank Fusion

**Files:**
- Modify: `crates/ninox-core/src/embeddings.rs`

**Interfaces:**
- Consumes: nothing from Task 1 beyond the module already existing.
- Produces: `pub fn reciprocal_rank_fusion(lists: &[Vec<String>], k: f64) -> Vec<(String, f64)>` — returns `(id, score)` pairs sorted by descending score. An id present in multiple lists sums its contribution from each.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/ninox-core/src/embeddings.rs`:

```rust
    #[test]
    fn rrf_single_list_preserves_order() {
        let lists = vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]];
        let fused = reciprocal_rank_fusion(&lists, 60.0);
        let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn rrf_boosts_ids_appearing_in_both_lists() {
        // "shared" is 2nd in list A and 3rd in list B; "a-only" is 1st in A
        // only. Appearing in both lists should outrank a single 1st-place
        // finish once contributions are summed.
        let list_a = vec!["a-only".to_string(), "shared".to_string()];
        let list_b = vec!["b-only-1".to_string(), "b-only-2".to_string(), "shared".to_string()];
        let fused = reciprocal_rank_fusion(&[list_a, list_b], 60.0);
        let top_id = &fused[0].0;
        assert_eq!(top_id, "shared");
    }

    #[test]
    fn rrf_empty_lists_returns_empty() {
        let fused = reciprocal_rank_fusion(&[], 60.0);
        assert!(fused.is_empty());
    }

    #[test]
    fn rrf_deduplicates_ids_within_a_single_list() {
        // Defensive: a caller should never pass duplicates within one list,
        // but the scoring must not double-count if it happens.
        let lists = vec![vec!["a".to_string(), "a".to_string()]];
        let fused = reciprocal_rank_fusion(&lists, 60.0);
        assert_eq!(fused.len(), 1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-core embeddings::tests::rrf 2>&1 | tail -20`
Expected: compile error, `reciprocal_rank_fusion` doesn't exist yet.

- [ ] **Step 3: Implement `reciprocal_rank_fusion`**

Add to `crates/ninox-core/src/embeddings.rs` (above the `tests` module):

```rust
use std::collections::HashMap;

/// Combine multiple independent rankings of the same ID space into one,
/// using `score(id) = Σ 1 / (k + rank)` over every list containing `id`
/// (rank is 1-based). This avoids comparing incomparable scales directly
/// (e.g. FTS5 BM25 vs. cosine similarity) — only rank position matters.
/// `k = 60` is the standard RRF constant. An id repeated within a single
/// list only counts its first (best) rank in that list.
pub fn reciprocal_rank_fusion(lists: &[Vec<String>], k: f64) -> Vec<(String, f64)> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    for list in lists {
        let mut seen_in_list: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (idx, id) in list.iter().enumerate() {
            if !seen_in_list.insert(id.as_str()) {
                continue;
            }
            let rank = (idx + 1) as f64;
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (k + rank);
        }
    }
    let mut scored: Vec<(String, f64)> = scores.into_iter().collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-core embeddings:: 2>&1 | tail -20`
Expected: `8 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/embeddings.rs
git commit -m "feat(brain): add reciprocal rank fusion for hybrid ranking"
```

---

## Task 3: `FastEmbedEmbedder`

**Files:**
- Modify: root `Cargo.toml`
- Modify: `crates/ninox-core/Cargo.toml`
- Modify: `crates/ninox-core/src/embeddings.rs`

**Interfaces:**
- Consumes: `Embedder` trait from Task 1.
- Produces: `pub struct FastEmbedEmbedder { .. }` with `pub fn try_new() -> anyhow::Result<Self>`, implementing `Embedder`.
- Produces: `pub const QUERY_INSTRUCTION_PREFIX: &str = "Represent this sentence for searching relevant passages: ";` (used by Task 6's query-time embedding, not by `FastEmbedEmbedder` itself — the trait stays model-convention-agnostic; the caller decides when to prepend it).

- [ ] **Step 1: Add the `fastembed` dependency**

In root `Cargo.toml`, add to `[workspace.dependencies]` (alphabetically, after `dirs`):

```toml
dirs        = "5"
fastembed   = "5"
```

In `crates/ninox-core/Cargo.toml`, add to `[dependencies]` (alphabetically, after `dirs`):

```toml
dirs       = { workspace = true }
fastembed  = { workspace = true }
```

- [ ] **Step 2: Write the ignored integration test**

This test needs the real ONNX runtime and downloads a ~90MB model on first run — it must never run in normal `cargo test`. This mirrors the existing `#[ignore]` pattern already used for `rebuild_scales_to_5000_files` in `crates/ninox-core/src/brain.rs:974`.

Add to the `tests` module in `crates/ninox-core/src/embeddings.rs`:

```rust
    #[test]
    #[ignore = "downloads a real model and runs ONNX inference: run explicitly with `cargo test -p ninox-core --release -- --ignored fast_embed_embedder_produces_384_dim_vectors -- --nocapture`"]
    fn fast_embed_embedder_produces_384_dim_vectors() {
        let embedder = FastEmbedEmbedder::try_new().expect("model should load");
        assert_eq!(embedder.dimension(), 384);

        let vec = embedder.embed("hello world").expect("embed should succeed");
        assert_eq!(vec.len(), 384);

        let batch = embedder
            .embed_batch(&["first".to_string(), "second".to_string()])
            .expect("batch embed should succeed");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].len(), 384);
    }

    #[test]
    #[ignore = "downloads a real model and runs ONNX inference: run explicitly with `cargo test -p ninox-core --release -- --ignored fast_embed_embedder_similar_text_scores_higher_than_unrelated -- --nocapture`"]
    fn fast_embed_embedder_similar_text_scores_higher_than_unrelated() {
        let embedder = FastEmbedEmbedder::try_new().expect("model should load");
        let query = embedder
            .embed(&format!("{QUERY_INSTRUCTION_PREFIX}auth failures"))
            .unwrap();
        let related = embedder.embed("401 debugging notes").unwrap();
        let unrelated = embedder.embed("chocolate chip cookie recipe").unwrap();

        let sim_related = cosine_similarity(&query, &related);
        let sim_unrelated = cosine_similarity(&query, &unrelated);
        assert!(
            sim_related > sim_unrelated,
            "expected related text to score higher: related={sim_related}, unrelated={sim_unrelated}"
        );
    }
```

- [ ] **Step 2b: Run it once to confirm it currently fails to compile**

Run: `cargo test -p ninox-core embeddings::tests::fast_embed -- --ignored 2>&1 | tail -20`
Expected: compile error, `FastEmbedEmbedder` and `QUERY_INSTRUCTION_PREFIX` don't exist yet.

- [ ] **Step 3: Implement `FastEmbedEmbedder`**

Add to `crates/ninox-core/src/embeddings.rs` (above the `tests` module):

```rust
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::{path::PathBuf, sync::Mutex};

/// Instruction prefix Arctic Embed (and BGE-family models) expect on
/// *query* text for asymmetric retrieval. Passage/entry text is embedded
/// with no prefix. This lives here, not inside `FastEmbedEmbedder`, so the
/// `Embedder` trait stays a plain "text in, vector out" abstraction —
/// the model-specific convention is the caller's concern (see `brain.rs`'s
/// hybrid query implementation).
pub const QUERY_INSTRUCTION_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// `fastembed`-backed [`Embedder`] using `snowflake-arctic-embed-xs`
/// (22M params, 384-dim) — see the design spec for why this model was
/// chosen over `all-MiniLM-L6-v2` and over hand-rolling inference with
/// `candle`.
pub struct FastEmbedEmbedder {
    model: Mutex<TextEmbedding>,
}

impl FastEmbedEmbedder {
    /// Loads the model, downloading and caching it under
    /// `{cache_dir}/ninox/fastembed` on first use. Every call after the
    /// first (across process restarts) is fully offline.
    pub fn try_new() -> Result<Self> {
        let cache_dir = fastembed_cache_dir();
        let model = TextEmbedding::try_new(
            TextInitOptions::new(EmbeddingModel::SnowflakeArcticEmbedXS)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )?;
        Ok(Self { model: Mutex::new(model) })
    }
}

fn fastembed_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ninox")
        .join("fastembed")
}

impl Embedder for FastEmbedEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut model = self.model.lock().unwrap();
        let mut out = model.embed(vec![text], None)?;
        Ok(out.remove(0))
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut model = self.model.lock().unwrap();
        model.embed(texts, None)
    }

    fn dimension(&self) -> usize {
        384
    }
}
```

- [ ] **Step 4: Run the full (non-ignored) test suite to confirm nothing broke**

Run: `cargo test -p ninox-core 2>&1 | tail -20`
Expected: all previously-passing tests still pass; the two new tests report `ignored`.

- [ ] **Step 5: Manually run the ignored tests once to confirm the real model works**

Run: `cargo test -p ninox-core --release -- --ignored fast_embed --nocapture 2>&1 | tail -40`
Expected: both tests pass (first run downloads the model — may take a minute; subsequent runs are fast). If this fails, stop and diagnose before continuing — every later task assumes this integration actually works.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/ninox-core/Cargo.toml crates/ninox-core/src/embeddings.rs Cargo.lock
git commit -m "feat(brain): add FastEmbedEmbedder using snowflake-arctic-embed-xs"
```

---

## Task 4: `embeddings` table and `RebuildStats`

**Files:**
- Modify: `crates/ninox-core/src/brain.rs`

**Interfaces:**
- Produces: `pub struct RebuildStats { pub indexed: usize, pub embedded: usize, pub cached: usize }`
- Produces: `embeddings` SQLite table, created in `BrainIndex::open()`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/ninox-core/src/brain.rs` (near `open_creates_schema`, around line 640):

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ninox-core brain::tests::open_creates_embeddings_table 2>&1 | tail -20`
Expected: FAIL — `no such table: embeddings`.

- [ ] **Step 3: Add the table to the schema**

In `crates/ninox-core/src/brain.rs`, modify the `execute_batch` call inside `open()` (around line 51-67):

```rust
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
```

Note: this table is deliberately absent from `rebuild()`'s `DELETE FROM entries; DELETE FROM entries_fts; DELETE FROM links;` statement (Task 5) — that is what makes embedding caching possible.

- [ ] **Step 4: Add `RebuildStats`**

Add near `QueryFilters` in `crates/ninox-core/src/brain.rs` (around line 32):

```rust
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
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p ninox-core brain::tests::open_creates_embeddings_table 2>&1 | tail -20`
Expected: PASS. (`RebuildStats` isn't used yet — that's fine, `cargo test` will warn about dead code until Task 5 wires it up; do not silence the warning, it will resolve itself next task.)

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/brain.rs
git commit -m "feat(brain): add embeddings table and RebuildStats"
```

---

## Task 5: Embedding pass in `rebuild()`

**Files:**
- Modify: `crates/ninox-core/src/brain.rs`

**Interfaces:**
- Consumes: `Embedder` trait (Task 1), `RebuildStats` (Task 4).
- Produces: `pub fn rebuild(&self, embedder: Option<&dyn Embedder>) -> Result<RebuildStats>` (signature change from `pub fn rebuild(&self) -> Result<usize>`).

This is the biggest task in the plan — it changes a public signature used by every other crate, so every existing caller is fixed in the same commit as the implementation to keep the tree buildable throughout.

- [ ] **Step 1: Write the failing tests for caching behavior**

Add to the `tests` module in `crates/ninox-core/src/brain.rs`. This needs a `FakeEmbedder` that counts calls — add it once, near `make_brain`:

```rust
    use crate::embeddings::Embedder;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p ninox-core brain::tests::rebuild_embeds 2>&1 | tail -20`
Expected: compile error — `rebuild(Some(&embedder))` doesn't match the current `rebuild()` signature.

- [ ] **Step 3: Implement the embedding pass**

Replace the `rebuild()` method in `crates/ninox-core/src/brain.rs` (currently lines 94-146):

```rust
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
```

Add these free functions near `row_to_entry` (they're plain helpers, not tied to `BrainIndex`):

```rust
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
```

Add the import at the top of the file:

```rust
use crate::embeddings::Embedder;
```

`blob_to_vector` isn't called yet — that's Task 6. Leave the `#[allow(dead_code)]` warning as-is; it resolves next task.

- [ ] **Step 4: Fix every other caller in the same commit**

The signature change from `rebuild() -> Result<usize>` to `rebuild(embedder: Option<&dyn Embedder>) -> Result<RebuildStats>` breaks every existing call site. Fix all of them now so the tree builds:

In `crates/ninox-core/src/brain.rs`'s own test module, every existing `brain.rebuild().unwrap()` becomes `brain.rebuild(None).unwrap()` (these tests don't test embedding behavior, so `None` is correct — keyword-only is exactly what they're checking). Apply this to these exact locations (re-check line numbers after Step 1's insertions shift them; search for the literal text instead):

```rust
// Before:
let count = brain.rebuild().unwrap();
// After:
let stats = brain.rebuild(None).unwrap();
```
— in `rebuild_indexes_files` (`assert_eq!(count, 2)` becomes `assert_eq!(stats.indexed, 2)`).

```rust
// Before:
brain.rebuild().unwrap();
// After:
brain.rebuild(None).unwrap();
```
— in `query_returns_matches`, `query_tolerates_special_characters`, `query_filters_by_type`, and every other test in this file that calls `.rebuild()` with no assertion on the return value (grep for `brain.rebuild()` and `brain_a.rebuild()` / `BrainIndex::open(&dir_b).unwrap().rebuild()` to find every remaining one in this file).

```rust
// Before:
fn rebuild_scales_to_500_files_within_ceiling() {
    ...
    let count = brain.rebuild().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(count, 500);
    ...
}
// After:
fn rebuild_scales_to_500_files_within_ceiling() {
    ...
    let stats = brain.rebuild(None).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(stats.indexed, 500);
    ...
}
```
— same transformation for `rebuild_scales_to_5000_files` (`assert_eq!(count, 5_000)` → `assert_eq!(stats.indexed, 5_000)`).

In `crates/ninox-app/src/main.rs`, `run_brain`'s `BrainAction::Index` arm (around line 302-305):

```rust
// Before:
BrainAction::Index => {
    let count = brain.rebuild()?;
    println!("indexed {count} entries");
}
// After:
BrainAction::Index => {
    let stats = brain.rebuild(None)?;
    println!("indexed {} entries", stats.indexed);
}
```
(Task 7 replaces this `None` with real lazy embedder construction — for now, just make it compile with identical behavior to today.)

In `crates/ninox-server/src/routes/brain.rs`'s `rebuild_index` (around line 24-34):

```rust
// Before:
async fn rebuild_index(
    State(brain): State<Arc<BrainIndex>>,
) -> Result<Json<IndexResponse>, StatusCode> {
    match brain.rebuild() {
        Ok(count) => Ok(Json(IndexResponse { count })),
        Err(err) => {
            tracing::error!("brain rebuild: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
// After:
async fn rebuild_index(
    State(brain): State<Arc<BrainIndex>>,
) -> Result<Json<IndexResponse>, StatusCode> {
    match brain.rebuild(None) {
        Ok(stats) => Ok(Json(IndexResponse { count: stats.indexed })),
        Err(err) => {
            tracing::error!("brain rebuild: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
```
(Task 8 wires this to a real embedder — for now, just make it compile with identical behavior to today.)

In `crates/ninox-app/src/app.rs`, every `<expr>.rebuild()` call (both production code and its own test module — grep for `.rebuild()` in this file) becomes `<expr>.rebuild(None)`. None of these call sites use the return value for anything other than `Ok`/`Err` matching or `.unwrap()`, so no further changes are needed at these sites. (Task 9 covers documenting why the GUI stays keyword-only for now.)

- [ ] **Step 5: Run the full workspace build to confirm every call site compiles**

Run: `cargo build --workspace 2>&1 | tail -40`
Expected: builds cleanly, no errors. (Warnings about unused `blob_to_vector` are expected until Task 6.)

- [ ] **Step 6: Run the new and existing brain tests to verify they pass**

Run: `cargo test -p ninox-core brain:: 2>&1 | tail -40`
Expected: all pass, including the 6 new tests from Step 1.

- [ ] **Step 7: Run the full workspace test suite to confirm no other regressions**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-core/src/brain.rs crates/ninox-app/src/main.rs crates/ninox-server/src/routes/brain.rs crates/ninox-app/src/app.rs
git commit -m "feat(brain): embed new/changed entries during rebuild, cache by content hash"
```

---

## Task 6: Hybrid ranking in `query()`

**Files:**
- Modify: `crates/ninox-core/src/brain.rs`

**Interfaces:**
- Consumes: `Embedder`, `cosine_similarity`, `reciprocal_rank_fusion` (Tasks 1-2), `QUERY_INSTRUCTION_PREFIX` (Task 3), `blob_to_vector` (Task 5).
- Produces: `pub fn query(&self, text: &str, embedder: Option<&dyn Embedder>, filters: QueryFilters) -> Result<Vec<BrainEntry>>` (signature change — `embedder` inserted as the second parameter).

- [ ] **Step 1: Write the failing test proving hybrid fusion actually works**

Add to the `tests` module in `crates/ninox-core/src/brain.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p ninox-core brain::tests::query_surfaces_semantic 2>&1 | tail -20`
Expected: compile error — `query(text, embedder, filters)` doesn't match the current signature.

- [ ] **Step 3: Implement hybrid ranking**

Replace the `query()` method in `crates/ninox-core/src/brain.rs` (currently the method starting `pub fn query(&self, text: &str, filters: QueryFilters)`):

```rust
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
```

Add the import at the top of the file:

```rust
use crate::embeddings::{cosine_similarity, reciprocal_rank_fusion, Embedder, QUERY_INSTRUCTION_PREFIX};
```

(Replace the earlier `use crate::embeddings::Embedder;` from Task 5 with this combined import.)

- [ ] **Step 4: Fix every other caller in the same commit**

Same reasoning as Task 5, Step 4 — fix every call site now so the tree builds.

In `crates/ninox-core/src/brain.rs`'s own test module, every `brain.query(<text>, QueryFilters { .. })` or `brain.query(<text>, QueryFilters::default())` becomes `brain.query(<text>, None, QueryFilters { .. })` / `brain.query(<text>, None, QueryFilters::default())` — except the four tests written in Step 1, which already use the new signature. This includes (at minimum) `query_returns_matches`, `query_filters_by_type`, and `query_tolerates_special_characters` — grep for `.query(` in this file's test module to find every remaining one.

In `crates/ninox-app/src/main.rs`, `run_brain`'s `BrainAction::Query` arm (around line 306-312):

```rust
// Before:
BrainAction::Query { text, entry_type, tag } => {
    let filters = QueryFilters { entry_type, tag };
    let entries = brain.query(&text, filters)?;
    for entry in &entries {
        println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
    }
}
// After:
BrainAction::Query { text, entry_type, tag } => {
    let filters = QueryFilters { entry_type, tag };
    let entries = brain.query(&text, None, filters)?;
    for entry in &entries {
        println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
    }
}
```
(Task 7 replaces this `None` with real lazy embedder construction.)

In `crates/ninox-server/src/routes/brain.rs`'s `query_entries` (around line 44-60):

```rust
// Before:
    match brain.query(&text, filters) {
// After:
    match brain.query(&text, None, filters) {
```
(Task 8 wires this to a real embedder.)

In `crates/ninox-app/src/app.rs`, every `state.brain.query("", QueryFilters::default())` call (grep for `.query(` in this file, both production code and tests) becomes `state.brain.query("", None, QueryFilters::default())`.

- [ ] **Step 5: Run the full workspace build to confirm every call site compiles**

Run: `cargo build --workspace 2>&1 | tail -40`
Expected: builds cleanly, no errors, no more dead-code warning for `blob_to_vector`.

- [ ] **Step 6: Run the new and existing brain tests to verify they pass**

Run: `cargo test -p ninox-core brain:: 2>&1 | tail -60`
Expected: all pass, including the 4 new tests from Step 1.

- [ ] **Step 7: Run the full workspace test suite to confirm no regressions**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-core/src/brain.rs crates/ninox-app/src/main.rs crates/ninox-server/src/routes/brain.rs crates/ninox-app/src/app.rs
git commit -m "feat(brain): hybrid keyword + semantic ranking via reciprocal rank fusion"
```

---

## Task 7: Wire the CLI to a real embedder

**Files:**
- Modify: `crates/ninox-app/src/main.rs`

**Interfaces:**
- Consumes: `FastEmbedEmbedder` (Task 3), `rebuild`/`query` (Tasks 5-6).

- [ ] **Step 1: Add lazy embedder construction to `run_brain`**

In `crates/ninox-app/src/main.rs`, replace the whole `run_brain` function:

```rust
async fn run_brain(action: BrainAction) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let brain_path = config.resolved_brain_path();
    let brain = BrainIndex::open(&brain_path)?;

    match action {
        BrainAction::Index => {
            let embedder = try_build_embedder();
            let stats = brain.rebuild(embedder.as_deref())?;
            println!(
                "indexed {} entries ({} embedded, {} cached)",
                stats.indexed, stats.embedded, stats.cached
            );
        }
        BrainAction::Query { text, entry_type, tag } => {
            let embedder = if text.trim().is_empty() { None } else { try_build_embedder() };
            let filters = QueryFilters { entry_type, tag };
            let entries = brain.query(&text, embedder.as_deref(), filters)?;
            for entry in &entries {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Show { path } => {
            match brain.get(&path)? {
                Some(entry) => println!("{}", serde_json::to_string_pretty(&entry)?),
                None => {
                    eprintln!("entry not found: {path}");
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}

/// Attempt to construct the local embedding model, falling back to `None`
/// (keyword-only search) on any failure — offline first run, corrupted
/// model cache, unsupported platform, etc. Embedding is an enhancement
/// layer; it must never be a hard dependency of `brain index`/`brain query`.
fn try_build_embedder() -> Option<Arc<dyn ninox_core::embeddings::Embedder>> {
    match ninox_core::embeddings::FastEmbedEmbedder::try_new() {
        Ok(embedder) => Some(Arc::new(embedder)),
        Err(err) => {
            tracing::warn!("brain: embedding model unavailable, falling back to keyword-only search: {err}");
            None
        }
    }
}
```

- [ ] **Step 2: Build to confirm it compiles**

Run: `cargo build -p ninox --bin ninox 2>&1 | tail -30`
Expected: builds cleanly.

- [ ] **Step 3: Manually verify end-to-end against a scratch brain**

```bash
SCRATCH=$(mktemp -d)
mkdir -p "$SCRATCH/notes"
cat > "$SCRATCH/notes/outage.md" <<'EOF'
---
name: 401 debugging notes
---
How we diagnosed a spate of 401s in the API gateway.
EOF
cat > "$SCRATCH/notes/coffee.md" <<'EOF'
---
name: Coffee notes
---
Best beans for espresso.
EOF
NINOX_BRAIN="$SCRATCH" ./target/debug/ninox brain index
NINOX_BRAIN="$SCRATCH" ./target/debug/ninox brain query "auth failures"
```

Expected: `brain index` reports `indexed 2 entries (2 embedded, 0 cached)` (first run downloads the model — allow a minute). `brain query "auth failures"` lists `401 debugging notes` even though it shares no words with the query, and `Coffee notes` should not outrank it. Re-run `brain index` a second time with no file changes and confirm it now reports `(0 embedded, 2 cached)`.

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-app/src/main.rs
git commit -m "feat(brain): wire ninox brain index/query to the local embedder"
```

---

## Task 8: Wire the HTTP route to a real embedder

**Files:**
- Modify: `crates/ninox-server/src/routes/brain.rs`
- Modify: `crates/ninox-server/src/server.rs`
- Modify: `crates/ninox-app/src/main.rs`

**Interfaces:**
- Consumes: `FastEmbedEmbedder` (Task 3).
- Produces: `pub async fn start(engine: Arc<Engine>, brain: Arc<BrainIndex>, embedder: Option<Arc<dyn Embedder>>, port: u16) -> anyhow::Result<()>` (signature change).

Unlike the CLI, the server is a long-running process — constructing the embedder per-request would mean re-loading the model (or worse, re-attempting a failed load) on every call. It's constructed once at server startup instead, off the async runtime via `spawn_blocking` so a slow first-time model download doesn't block anything else. This is a deliberate, disclosed departure from the CLI's per-invocation laziness — the *principle* it preserves is "an empty/filter-only query never pays an embedding cost" and "a missing/broken model never breaks the route," both of which still hold.

- [ ] **Step 1: Add a combined state struct and thread the embedder through the router**

Replace the contents of `crates/ninox-server/src/routes/brain.rs`:

```rust
use ninox_core::{embeddings::Embedder, BrainEntry, BrainIndex, QueryFilters};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct BrainState {
    pub brain: Arc<BrainIndex>,
    pub embedder: Option<Arc<dyn Embedder>>,
}

pub fn brain_router(brain: Arc<BrainIndex>, embedder: Option<Arc<dyn Embedder>>) -> Router {
    Router::new()
        .route("/index", post(rebuild_index))
        .route("/query", get(query_entries))
        .route("/entry/*path", get(get_entry))
        .with_state(BrainState { brain, embedder })
}

#[derive(Serialize)]
struct IndexResponse {
    count: usize,
    embedded: usize,
    cached: usize,
}

async fn rebuild_index(
    State(state): State<BrainState>,
) -> Result<Json<IndexResponse>, StatusCode> {
    match state.brain.rebuild(state.embedder.as_deref()) {
        Ok(stats) => Ok(Json(IndexResponse {
            count: stats.indexed,
            embedded: stats.embedded,
            cached: stats.cached,
        })),
        Err(err) => {
            tracing::error!("brain rebuild: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(Deserialize)]
struct QueryParams {
    q: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    tag: Option<String>,
}

async fn query_entries(
    State(state): State<BrainState>,
    Query(params): Query<QueryParams>,
) -> Result<Json<Vec<BrainEntry>>, StatusCode> {
    let text = params.q.unwrap_or_default();
    let filters = QueryFilters {
        entry_type: params.entry_type,
        tag: params.tag,
    };
    match state.brain.query(&text, state.embedder.as_deref(), filters) {
        Ok(entries) => Ok(Json(entries)),
        Err(err) => {
            tracing::error!("brain query: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_entry(
    State(state): State<BrainState>,
    Path(path): Path<String>,
) -> Result<Json<BrainEntry>, StatusCode> {
    match state.brain.get(&path) {
        Ok(Some(entry)) => Ok(Json(entry)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(err) => {
            tracing::error!("brain get {path}: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
```

- [ ] **Step 2: Update `server::start` to accept and thread the embedder**

In `crates/ninox-server/src/server.rs`, replace the whole file:

```rust
use crate::routes::{
    brain::brain_router,
    events::events_router,
    orchestrators::orchestrators_router,
    sessions::sessions_router,
    terminal::terminal_router,
};
use ninox_core::{embeddings::Embedder, events::Engine, BrainIndex};
use axum::Router;
use std::{net::SocketAddr, sync::Arc};
use tower_http::cors::CorsLayer;

pub async fn start(
    engine: Arc<Engine>,
    brain: Arc<BrainIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    port: u16,
) -> anyhow::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let app = Router::new()
        .nest("/api/v1/sessions", sessions_router(engine.clone()))
        .nest("/api/v1/sessions", terminal_router(engine.clone()))
        .nest("/api/v1/orchestrators", orchestrators_router(engine.clone()))
        .nest("/api/v1/events", events_router(engine.clone()))
        .nest("/api/brain", brain_router(brain, embedder))
        .layer(CorsLayer::permissive());
    tracing::info!("ninox listening on {addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
```

- [ ] **Step 3: Construct the embedder at startup in `run_tui`**

In `crates/ninox-app/src/main.rs`, in `run_tui` (around line 327-365), find:

```rust
    let brain = Arc::new(BrainIndex::open(&brain_path)?);
```

and, immediately after it, add:

```rust
    let embedder: Option<Arc<dyn ninox_core::embeddings::Embedder>> =
        match tokio::task::spawn_blocking(ninox_core::embeddings::FastEmbedEmbedder::try_new).await {
            Ok(Ok(embedder)) => Some(Arc::new(embedder) as Arc<dyn ninox_core::embeddings::Embedder>),
            Ok(Err(err)) => {
                tracing::warn!("brain: embedding model unavailable, semantic search disabled: {err}");
                None
            }
            Err(join_err) => {
                tracing::warn!("brain: embedder init task panicked: {join_err}");
                None
            }
        };
```

Then find the existing server spawn:

```rust
    tokio::spawn({
        let e = engine.clone();
        let b = brain.clone();
        async move {
            if let Err(err) = ninox_server::start(e, b, port).await {
                tracing::error!("server: {err}");
            }
        }
    });
```

and change it to:

```rust
    tokio::spawn({
        let e = engine.clone();
        let b = brain.clone();
        let emb = embedder.clone();
        async move {
            if let Err(err) = ninox_server::start(e, b, emb, port).await {
                tracing::error!("server: {err}");
            }
        }
    });
```

- [ ] **Step 4: Build to confirm everything compiles**

Run: `cargo build --workspace 2>&1 | tail -40`
Expected: builds cleanly.

- [ ] **Step 5: Run the full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: all pass. (`ninox-server`'s existing route tests, if any exercise `brain_router` directly, will need `None` passed for `embedder` — check `crates/ninox-server/src/routes/brain.rs` and any test files under `crates/ninox-server` for `brain_router(` call sites and update them the same way as Task 5/6's call-site fixes.)

- [ ] **Step 6: Manually verify the HTTP route**

```bash
./target/debug/ninox &
SERVER_PID=$!
sleep 2
curl -s "http://127.0.0.1:PORT/api/brain/query?q=auth+failures" | head -c 500
kill $SERVER_PID
```

(Substitute the actual configured port — check `~/.config/ninox/config.toml` or the `ninox` startup log line `ninox listening on ...`.) Expected: JSON array including any brain entries semantically related to "auth failures," not just literal keyword matches.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-server/src/routes/brain.rs crates/ninox-server/src/server.rs crates/ninox-app/src/main.rs
git commit -m "feat(brain): construct embedder at server startup, wire into HTTP routes"
```

---

## Task 9: Skill doc update and GUI call-site documentation

**Files:**
- Modify: `crates/ninox-app/src/app.rs`

**Interfaces:**
- None new — this task documents an intentional scope boundary and updates user-facing skill text.

- [ ] **Step 1: Add a line to the brain skill's "Query first" section**

In `crates/ninox-app/src/app.rs`, inside the `brain_skill_content` string (the `r#"..."#` literal starting around line 2200), find the `## 1. Query first` section and add one sentence after its introductory line:

```
// Before:
## 1. Query first

Before writing a new entry, check whether one already exists:

// After:
## 1. Query first

`brain query` blends keyword and semantic matches automatically — no new
syntax needed. Before writing a new entry, check whether one already exists:
```

- [ ] **Step 2: Extend the existing skill test to check for this line**

In `crates/ninox-app/src/app.rs`'s `setup_orchestrator_root_seeds_brain_skill` test, add an assertion alongside the existing ones:

```rust
        assert!(skill.contains("ninox brain query"));
        assert!(skill.contains("ninox brain index"));
        assert!(skill.contains("ninox brain show"));
        assert!(skill.contains("blends keyword and semantic matches"));
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p ninox setup_orchestrator_root 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 4: Document why the GUI brain panel stays keyword-only**

Above each of the `.query("", None, QueryFilters::default())` and `.rebuild(None)` call sites added in Tasks 5-6 inside the `Message::update`/`Self::apply` match arms of `crates/ninox-app/src/app.rs` (around the `NavigateBrain`, and the rebuild/refresh handlers near lines 1622 and 1706 — search for `state.brain.query(` and `state.brain.rebuild(` in production code, not the test module), add a one-line comment:

```rust
// `None`: the GUI brain panel is keyword-only for now — wiring it to the
// same embedder the server constructs at startup is a natural follow-up,
// deliberately out of scope for the CLI/HTTP-focused semantic search spec.
```

- [ ] **Step 5: Run the full workspace test suite one more time**

Run: `cargo test --workspace 2>&1 | tail -40`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src/app.rs
git commit -m "docs(brain): document hybrid search in the brain skill"
```

---

## Task 10: Final verification pass

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace 2>&1 | tail -20`
Expected: clean build, no warnings about unused code from this feature.

- [ ] **Step 2: Full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all pass.

- [ ] **Step 3: Run the ignored `fastembed` integration tests once more**

Run: `cargo test -p ninox-core --release -- --ignored fast_embed --nocapture 2>&1 | tail -40`
Expected: both pass.

- [ ] **Step 4: Re-run the end-to-end CLI smoke test from Task 7, Step 3, from a clean scratch brain**

This confirms the whole feature works together after all later tasks' changes, not just in isolation after Task 7.

```bash
SCRATCH=$(mktemp -d)
mkdir -p "$SCRATCH/notes"
cat > "$SCRATCH/notes/outage.md" <<'EOF'
---
name: 401 debugging notes
---
How we diagnosed a spate of 401s in the API gateway.
EOF
cat > "$SCRATCH/notes/coffee.md" <<'EOF'
---
name: Coffee notes
---
Best beans for espresso.
EOF
NINOX_BRAIN="$SCRATCH" ./target/debug/ninox brain index
NINOX_BRAIN="$SCRATCH" ./target/debug/ninox brain query "auth failures"
NINOX_BRAIN="$SCRATCH" ./target/debug/ninox brain index  # should report cached, not re-embedded
```

Expected: same as Task 7 Step 3 — semantic match surfaces, second index run shows `(0 embedded, 2 cached)`.

- [ ] **Step 5: Confirm no dependency/build regressions in the other two binaries**

Run: `cargo build -p ninox-server 2>&1 | tail -20 && cargo build -p ninox-core 2>&1 | tail -20`
Expected: both clean.

This task has no commit — it's a checkpoint before calling the feature done. If any step fails, return to the task that introduced the regression.
