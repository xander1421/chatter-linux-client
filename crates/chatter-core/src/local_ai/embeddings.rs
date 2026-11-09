//! Embeddings.
//!
//! Local embedding generation using the configured backend (default: nomic-embed-text via Ollama).
//! Embeddings are used for semantic search over messages and memory retrieval.

use crate::ai_protocol::{BackendError, LocalAIBackend};
use super::settings::LocalAISettings;

/// Embedding generator using a local AI backend.
pub struct EmbeddingGenerator<B: LocalAIBackend> {
    backend: B,
    model: String,
    dimension: usize,
}

impl<B: LocalAIBackend> EmbeddingGenerator<B> {
    /// Create a new EmbeddingGenerator with the given backend and settings.
    pub fn new(backend: B, settings: &LocalAISettings) -> Self {
        Self {
            backend,
            model: settings.embedding_model.clone(),
            dimension: settings.embedding_dimension,
        }
    }

    /// Generate embeddings for a single text.
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, BackendError> {
        let results = self.backend.embed(&self.model, &[text.to_string()]).await?;

        results
            .into_iter()
            .next()
            .ok_or_else(|| BackendError::InvalidResponse("empty embedding response".into()))
    }

    /// Generate embeddings for multiple texts in a single batch.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        self.backend.embed(&self.model, texts).await
    }

    /// Compute cosine similarity between two embedding vectors.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }

        dot / (norm_a * norm_b)
    }

    /// Get the expected embedding dimension.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Get the model name used for embedding.
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim =
            EmbeddingGenerator::<super::super::ollama::OllamaBackend>::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim =
            EmbeddingGenerator::<super::super::ollama::OllamaBackend>::cosine_similarity(&a, &b);
        assert!(sim.abs() < f32::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim =
            EmbeddingGenerator::<super::super::ollama::OllamaBackend>::cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        let sim =
            EmbeddingGenerator::<super::super::ollama::OllamaBackend>::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 0.0];
        let sim =
            EmbeddingGenerator::<super::super::ollama::OllamaBackend>::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }
}
