# Brain: Hybrid Semantic Search

## Context

The brain (`crates/ninox-core/src/brain.rs`) is Ninox's persistent knowledge
store: Markdown files with YAML frontmatter, indexed into SQLite with an
FTS5 full-text table for `ninox brain query`. FTS5 is a pure keyword index —
it can only find entries that share vocabulary with the query text. A query
for `"auth failures"` will not surface an entry titled `"401 debugging
notes"` even though they're the same topic, because there's no shared
token.

This is one of three gaps identified while comparing the brain against
open-source "second brain" tools for AI agents (notably
[Basic Memory](https://github.com/basicmachines-co/basic-memory), which
uses hybrid full-text + vector search). It is being specced and built as
its own sub-project, sequenced first among the three:

1. **Hybrid semantic search** (this spec)
2. Expose existing graph traversal (`backlinks`/`outlinks`/`related`) via
   the CLI and document it in the brain skill
3. Structured write primitives (append/prepend/replace-section) instead of
   raw hand-editing

Sub-projects 2 and 3 are out of scope here and will get their own specs.

## Goal

`ninox brain query <text>` finds conceptually related entries even when
they share no vocabulary with the query, by blending the existing FTS5
keyword ranking with vector similarity from a local embedding model. This
is the default behavior of `brain query` — no new flag, nothing for
orchestrators to learn.

## Non-goals

- No `--limit` CLI flag (fixed top-20 cap for v1).
- No similarity threshold on the semantic leg (see "Query flow" below for
  why).
- No document chunking for long entries — embeddings are computed over a
  truncated prefix of each entry.
- No reranking model, no ANN/vector index (brute-force cosine is fast
  enough at the scale ninox already tests for — see "Vector storage"
  below).
- No changes to graph traversal or write commands (sub-projects 2 and 3).

## Approach

Use the [`fastembed`](https://github.com/Anush008/fastembed-rs) crate
(v5), which wraps ONNX Runtime (via `ort`) for local CPU inference. This
was chosen over hand-rolling inference with `candle`: `fastembed` is a
~10-line integration (`TextEmbedding::try_new(...)`,
`.embed(texts, None)`), sync (no Tokio conflict), and its model-loading/
tokenization/pooling is battle-tested — a hand-rolled pipeline risks
silently-wrong embeddings (subtly bad pooling still "works," it's just
worse, and is hard to unit-test meaningfully). The accepted trade-off is a
transitive C++ ONNX Runtime binary dependency, which `ort` auto-downloads
per-platform at build time (no system ONNX install required; works on
macOS arm64 and Ubuntu x86_64 CI without extra setup).

**Model: `snowflake-arctic-embed-xs`**, not `all-MiniLM-L6-v2`. Both are
22M-parameter, 384-dim models with effectively identical size and
inference latency, but Arctic Embed XS is a retrieval-tuned successor to
MiniLM (built on the same architecture, trained specifically for
query/passage retrieval) and scores meaningfully higher on the MTEB/BEIR
retrieval benchmark: **50.15 NDCG@10 vs. 41.95** for
all-MiniLM-L6-v2[^1]. As of 2026, MiniLM-L6-v2 (2021-era) is considered
fine for prototyping but outdated for production retrieval; Arctic Embed
is purpose-built for exactly this use case at zero extra size/latency
cost. (`snowflake-arctic-embed-s`, 33M params, scores marginally higher
still at 51.98 — not worth the extra size for this use case, but available
as a one-enum-value swap later if quality ever needs to move.)

Arctic Embed follows the standard asymmetric-retrieval convention: **only
query text** gets the instruction prefix `"Represent this sentence for
searching relevant passages: "` before embedding; entry/passage text is
embedded with no prefix. `fastembed`'s API is a thin ONNX wrapper (not
`sentence-transformers`), so this prefixing is not automatic — ninox's
code applies it explicitly at the two embedding call sites (query-time vs.
index-time).

[^1]: [Snowflake-Labs/arctic-embed](https://github.com/Snowflake-Labs/arctic-embed) —
    model card comparison table.

The model itself is fetched from Hugging Face and cached locally
(`dirs::cache_dir()/ninox/fastembed`) the first time it's actually needed;
every run after that is fully offline. This was an explicit user choice
over requiring 100%-offline-from-first-use (which would mean bundling a
model in the binary) or calling a hosted embeddings API (rejected — adds a
required API key and a per-rebuild network dependency, inconsistent with
the brain's local-first design).

### Vector storage: is SQLite the right store?

Yes, for this scale. A vector is `384 × 4 bytes = 1536 bytes`; at the
5,000-entry ceiling `rebuild()` is already tested against
(`rebuild_scales_to_5000_files`), all vectors together are ~7.5MB — loaded
into memory and brute-force compared in a plain Rust loop in low
single-digit milliseconds (a 384-float dot product is ~384 FLOPs; 5,000 of
them is under 2M FLOPs total, dominated by SQLite I/O, not the math).
Dedicated vector-search SQLite extensions exist — notably
[`sqlite-vec`](https://github.com/asg017/sqlite-vec) (SIMD-accelerated
distance functions, brute-force under the hood at this scale anyway) and
[`sqlite-vector-rs`](https://crates.io/crates/sqlite-vector-rs) (HNSW
approximate nearest-neighbor via `usearch`) — but both exist to solve a
problem ninox doesn't have: brute force stops being "fast enough" somewhere
in the tens-of-thousands-to-millions-of-vectors range, and the brain's own
design intent (`docs/BRAIN.md`) is incremental, human-curated knowledge,
not a bulk corpus. Adding either would mean a *second* native-binary
dependency (on top of `ort`) for no measurable benefit today.
The plain-`BLOB`-column schema is also not a dead end: it stores exactly
the `(id, vector)` pairs `sqlite-vec` itself would need, so migrating to a
vector-search extension later, if the brain ever grows far beyond its
intended scale, is a storage-layer swap, not a redesign.

## Architecture

### `Embedder` trait

New module `crates/ninox-core/src/embeddings.rs`:

```rust
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}
```

`FastEmbedEmbedder` implements it, wrapping `fastembed::TextEmbedding`.
This indirection is what makes the rest of the system (and all tests)
independent of the real model — tests use a `FakeEmbedder` that returns
deterministic vectors with no network access and no ONNX runtime.

`BrainIndex` does **not** own an embedder as internal state. `rebuild()`
and `query()` each take `embedder: Option<&dyn Embedder>` as a parameter.
The caller (CLI or HTTP route) owns the embedder's lifecycle and decides
when — or whether — to construct one; this is what makes "lazy, only when
actually about to embed" simple: for `brain query`, the caller only
attempts construction when `text` is non-empty, so an empty/filter-only
query never touches the model, with no internal laziness logic needed
inside `BrainIndex` at all. `None` means "embeddings unavailable" for this
call (construction failed, skipped, or text was empty) — every code path
that touches embeddings degrades to today's exact keyword-only behavior
when this is `None`. Embedding failures never block indexing or querying.
Tests pass `Some(&FakeEmbedder)` or `None` directly — no special
`BrainIndex` constructor needed.

### Storage

New table, additive to the existing schema and **not** touched by
`rebuild()`'s `DELETE FROM entries` transaction — this is what makes
caching possible:

```sql
CREATE TABLE IF NOT EXISTS embeddings (
    id            TEXT PRIMARY KEY,   -- same id as entries.id
    content_hash  INTEGER NOT NULL,   -- hash of the text this vector was computed from
    vector        BLOB NOT NULL       -- 384 × f32, little-endian
);
```

`content_hash` uses `std::hash::Hasher` (e.g. `DefaultHasher`) — not
cryptographic, purely for change detection. It is not guaranteed stable
across Rust/std versions; the worst case of that is one redundant
re-embed after an upgrade, never a correctness problem, so no extra
dependency is justified here.

### Index/rebuild flow

`rebuild()` gains an embedding pass around the existing file-walk/parse/
insert logic:

1. After parsing `records: Vec<FileRecord>` as today, compute
   `content_hash` per record from `"{name}\n\n{body}"` truncated to ~2000
   characters (small models like MiniLM have a limited token context;
   truncating to a prefix is a pragmatic v1 choice, not a correctness
   guarantee of the whole document being represented).
2. Look up each `id` in `embeddings`. Matching `content_hash` → reuse the
   cached vector, no recompute. Missing or mismatched → queue `(id, text)`
   for embedding.
3. Run all queued texts through one `embedder.embed_batch(...)` call
   (batching is significantly faster than one-at-a-time), then upsert
   results into `embeddings`.
4. Prune `embeddings` rows whose `id` no longer exists in `entries`
   (`DELETE ... WHERE id NOT IN (SELECT id FROM entries)`).
5. If no embedder is available, skip steps 1–4 entirely and log a warning
   once. `entries`/FTS indexing is completely unaffected.

**Signature change (contained to this repo):** `rebuild()` returns
`Result<RebuildStats>` instead of `Result<usize>`, where:

```rust
pub struct RebuildStats {
    pub indexed:  usize,  // entries indexed (was the whole return value before)
    pub embedded: usize,  // newly computed this run
    pub cached:   usize,  // reused from the embeddings table
}
```

`ninox brain index` prints `"indexed 42 entries (3 embedded, 39 cached)"`.
This touches the existing tests that assert on the old `usize` return
(`rebuild_indexes_files`, `rebuild_scales_to_500_files_within_ceiling`,
`rebuild_scales_to_5000_files`) and the one CLI call site — all updated as
part of this work.

### Query flow

`query(text, filters)`, when `text` is non-empty and an embedder is
available:

1. **Keyword leg (A):** existing FTS5 `MATCH` search, unchanged.
2. **Semantic leg (B):** embed the query text once, load all `(id,
   vector)` pairs from `embeddings`, compute cosine similarity against
   each, sort descending, take the top 20. Brute-force is deliberate — at
   the ~500–5000 entry scale `rebuild()` is already tested against (see
   `rebuild_scales_to_500_files_within_ceiling` /
   `rebuild_scales_to_5000_files`), a full in-memory scan is a few
   milliseconds. No ANN index needed.
3. **Fuse A and B with Reciprocal Rank Fusion:**
   `score(id) = Σ 1/(60 + rank_in_list)` over whichever list(s) contain
   it. This avoids comparing BM25 scores against cosine similarities
   directly (different, incomparable scales) — RRF only uses rank
   position.
4. **Filter** the fused, deduped list by `entry_type`/`tag`, exactly as
   today (post-filter).
5. **Cap** to a final top 20.

If `text` is empty, or no embedder is available, behavior is **exactly
today's** — pure FTS5, or the existing filter-only path. No similarity
threshold is applied to the semantic leg: nearest-neighbor search always
returns *something* ranked by relative closeness, which is normal for
this kind of search. A hard cosine cutoff is a fragile magic number to
pick without real usage data; if noisy semantic-only results turn out to
be a problem in practice, a threshold is a cheap, isolated follow-up.

## Error handling

Embedding is treated as a soft-fail enhancement layer, never a hard
dependency of the brain:

- Model load failure (offline on first run, corrupted cache, unsupported
  platform) → log a warning once, `Embedder` becomes `None`, both
  `rebuild()` and `query()` behave exactly as they do today.
- Embedder construction happens lazily, only when something is actually
  about to be embedded — an empty/filter-only query never triggers a
  model load.

## Wiring

- Add `fastembed = "5"` to `[workspace.dependencies]` in the root
  `Cargo.toml`, referenced with `workspace = true` from `ninox-core`.
- `ninox brain index` (CLI) and the `ninox-server`
  `GET /api/brain/query` route each construct a `FastEmbedEmbedder`
  lazily, per the error-handling rule above.
- The brain skill (`crates/ninox-app/src/app.rs`,
  `setup_orchestrator_root`) gets one added line: `brain query` blends
  keyword and semantic matches automatically. No new syntax for
  orchestrators.

## Testing

The `Embedder` trait means no test needs the real model, network access,
or the ONNX runtime:

- `FakeEmbedder`: deterministic vectors (e.g. derived from character
  n-grams), so tests can construct two strings that are "close" by
  construction.
- Caching: a second `rebuild()` with unchanged files makes zero calls to
  the embedder (assert via a call-count spy on `FakeEmbedder`); a changed
  file triggers exactly one.
- RRF fusion: pure-function unit tests on synthetic rank lists — no DB, no
  embedder.
- Hybrid `query()`: a fixture where the query text shares no vocabulary
  with an entry but a close fake-embedding still surfaces it — proves the
  fusion path end to end.
- Degradation: an `Embedder` stub that always errors — `rebuild()` and
  `query()` must still succeed with keyword-only results, matching
  current behavior exactly.
- Existing brain tests (`query_returns_matches`, `query_filters_by_type`,
  `query_tolerates_special_characters`, the three `rebuild_*` tests) are
  updated for the `RebuildStats` signature change but must otherwise keep
  passing unmodified in intent — this is an additive feature, not a
  behavior change to keyword-only search.
