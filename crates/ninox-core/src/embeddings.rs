use anyhow::Result;
use std::collections::HashMap;

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
}
