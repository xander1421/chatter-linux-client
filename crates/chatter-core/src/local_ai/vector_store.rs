//! Vector store for local semantic search.
//!
//! Provides in-memory similarity search backed by SQLite persistence.
//! Uses brute-force cosine similarity — efficient for typical messaging
//! volumes (10K-50K vectors). No external vector DB dependency needed.

use std::sync::Arc;

use crate::error::StorageError;
use crate::storage::repositories::vector_repo::{
    EmbeddingSourceType, StoredEmbedding, VectorRepository,
};

/// A search result with relevance score.
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub source_id: String,
    pub source_type: EmbeddingSourceType,
    pub channel_id: Option<String>,
    pub content_preview: String,
    pub score: f32,
}

/// Vector store for similarity search over locally-stored embeddings.
///
/// This enables semantic search for E2E encrypted channels where the
/// server cannot search plaintext. Embeddings are generated client-side
/// via Ollama and stored in SQLite.
pub struct VectorStore {
    repo: Arc<VectorRepository>,
}

impl VectorStore {
    /// Create a new VectorStore backed by the given repository.
    pub fn new(repo: Arc<VectorRepository>) -> Self {
        Self { repo }
    }

    /// Store an embedding for a message or memory.
    pub fn store(&self, embedding: StoredEmbedding) -> Result<(), StorageError> {
        self.repo.upsert(&embedding)
    }

    /// Store multiple embeddings in a batch.
    pub fn store_batch(&self, embeddings: &[StoredEmbedding]) -> Result<u32, StorageError> {
        self.repo.upsert_batch(embeddings)
    }

    /// Check if a source has already been indexed.
    pub fn is_indexed(
        &self,
        source_type: EmbeddingSourceType,
        source_id: &str,
    ) -> Result<bool, StorageError> {
        self.repo.is_indexed(source_type, source_id)
    }

    /// Search within a specific channel using cosine similarity.
    ///
    /// Returns results sorted by descending relevance score.
    pub fn search_channel(
        &self,
        channel_id: &str,
        query_embedding: &[f32],
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<VectorSearchResult>, StorageError> {
        let embeddings = self
            .repo
            .load_channel_embeddings(channel_id, EmbeddingSourceType::Message)?;
        Ok(rank_results(&embeddings, query_embedding, limit, min_score))
    }

    /// Search across all channels using cosine similarity.
    pub fn search_global(
        &self,
        query_embedding: &[f32],
        source_type: EmbeddingSourceType,
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<VectorSearchResult>, StorageError> {
        let embeddings = self.repo.load_all_embeddings(source_type)?;
        Ok(rank_results(&embeddings, query_embedding, limit, min_score))
    }

    /// Delete embeddings for a source.
    pub fn delete(
        &self,
        source_type: EmbeddingSourceType,
        source_id: &str,
    ) -> Result<(), StorageError> {
        self.repo.delete_by_source(source_type, source_id)
    }

    /// Delete all embeddings for a channel.
    pub fn delete_channel(&self, channel_id: &str) -> Result<u32, StorageError> {
        self.repo.delete_channel_embeddings(channel_id)
    }

    /// Get the count of indexed items for a channel.
    pub fn count_channel(&self, channel_id: &str) -> Result<u32, StorageError> {
        self.repo.count_by_channel(channel_id)
    }

    /// Get total indexed item count.
    pub fn count_total(&self) -> Result<u32, StorageError> {
        self.repo.count_total()
    }
}

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// Rank stored embeddings by similarity to query, return top results.
fn rank_results(
    embeddings: &[StoredEmbedding],
    query: &[f32],
    limit: usize,
    min_score: f32,
) -> Vec<VectorSearchResult> {
    let mut scored: Vec<VectorSearchResult> = embeddings
        .iter()
        .map(|e| VectorSearchResult {
            source_id: e.source_id.clone(),
            source_type: e.source_type,
            channel_id: e.channel_id.clone(),
            content_preview: e.content_preview.clone(),
            score: cosine_similarity(&e.embedding, query),
        })
        .filter(|r| r.score >= min_score)
        .collect();

    // Sort descending by score
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn test_rank_results_filtering() {
        let embeddings = vec![
            StoredEmbedding {
                id: "1".into(),
                source_type: EmbeddingSourceType::Message,
                source_id: "msg1".into(),
                channel_id: Some("ch1".into()),
                content_preview: "hello world".into(),
                embedding: vec![1.0, 0.0, 0.0],
                model: "test".into(),
                dimension: 3,
            },
            StoredEmbedding {
                id: "2".into(),
                source_type: EmbeddingSourceType::Message,
                source_id: "msg2".into(),
                channel_id: Some("ch1".into()),
                content_preview: "goodbye".into(),
                embedding: vec![0.0, 1.0, 0.0],
                model: "test".into(),
                dimension: 3,
            },
        ];

        let query = vec![0.9, 0.1, 0.0];
        let results = rank_results(&embeddings, &query, 10, 0.0);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source_id, "msg1"); // closer to query
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_rank_results_min_score_filter() {
        let embeddings = vec![
            StoredEmbedding {
                id: "1".into(),
                source_type: EmbeddingSourceType::Message,
                source_id: "msg1".into(),
                channel_id: None,
                content_preview: "relevant".into(),
                embedding: vec![1.0, 0.0],
                model: "test".into(),
                dimension: 2,
            },
            StoredEmbedding {
                id: "2".into(),
                source_type: EmbeddingSourceType::Message,
                source_id: "msg2".into(),
                channel_id: None,
                content_preview: "irrelevant".into(),
                embedding: vec![0.0, 1.0],
                model: "test".into(),
                dimension: 2,
            },
        ];

        let query = vec![1.0, 0.0];
        let results = rank_results(&embeddings, &query, 10, 0.5);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_id, "msg1");
    }
}
