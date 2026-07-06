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
