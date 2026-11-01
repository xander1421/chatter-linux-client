//! Semantic search service.
//!
//! Embedding-based search using local AI. Coordinates between the
//! EmbeddingGenerator (Ollama) and VectorStore (SQLite) to provide
//! semantic search for E2E encrypted channels.

use std::sync::Arc;

use crate::ai_protocol::BackendError;
use crate::error::ServiceError;
use crate::local_ai::embeddings::EmbeddingGenerator;
use crate::local_ai::ollama::OllamaBackend;
use crate::local_ai::vector_store::{VectorSearchResult, VectorStore};
use crate::storage::repositories::vector_repo::{
    EmbeddingSourceType, StoredEmbedding, VectorRepository,
};

/// Semantic search service for local AI-powered message search.
///
/// This is the primary interface for semantic search operations.
/// It handles:
/// - Embedding generation via Ollama
/// - Vector storage/retrieval via SQLite
/// - Similarity ranking
pub struct SemanticSearchService {
    vector_store: Arc<VectorStore>,
    embedding_gen: Arc<EmbeddingGenerator<OllamaBackend>>,
    /// Optional keyword leg for hybrid retrieval (BM25 over messages_fts).
    keyword: Option<(
        Arc<crate::storage::repositories::SearchIndex>,
        Arc<crate::storage::repositories::MessageRepository>,
    )>,
}

impl SemanticSearchService {
    /// Create a new SemanticSearchService.
    pub fn new(
        vector_repo: Arc<VectorRepository>,
        embedding_gen: Arc<EmbeddingGenerator<OllamaBackend>>,
    ) -> Self {
        Self {
            vector_store: Arc::new(VectorStore::new(vector_repo)),
            embedding_gen,
            keyword: None,
        }
    }

    /// Enable the BM25 keyword leg (hybrid retrieval).
    pub fn with_keyword_index(
        mut self,
        search_index: Arc<crate::storage::repositories::SearchIndex>,
        message_repo: Arc<crate::storage::repositories::MessageRepository>,
    ) -> Self {
        self.keyword = Some((search_index, message_repo));
        self
    }

    /// Search messages within a channel by semantic similarity.
    ///
    /// Generates an embedding for the query text, then searches stored
    /// message embeddings using cosine similarity.
    pub async fn search_channel(
        &self,
        channel_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>, ServiceError> {
        let query_embedding = self
            .embedding_gen
            .embed_one(query)
            .await
            .map_err(backend_to_service)?;

        self.vector_store
            .search_channel(channel_id, &query_embedding, limit, 0.3)
            .map_err(ServiceError::Storage)
    }

    /// Search across all indexed messages.
    pub async fn search_global(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>, ServiceError> {
        let query_embedding = self
            .embedding_gen
            .embed_one(query)
            .await
            .map_err(backend_to_service)?;

        self.vector_store
            .search_global(&query_embedding, EmbeddingSourceType::Message, limit, 0.3)
            .map_err(ServiceError::Storage)
    }

    /// Hybrid retrieval: BM25 keyword leg + vector leg, merged by reciprocal
    /// rank fusion (k=60). Degrades gracefully — either leg may be absent
    /// (no embedding model → keyword-only; no keyword index → vector-only).
    pub async fn search_hybrid(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>, ServiceError> {
        let vector_hits = self.search_global(query, limit).await.unwrap_or_default();

        let keyword_hits: Vec<VectorSearchResult> = match &self.keyword {
            Some((index, messages)) => index
                .search(query, None, limit as u32)
                .map(|results| {
                    results
                        .into_iter()
                        .filter_map(|r| {
                            let msg = messages.get(&r.message_id).ok().flatten()?;
                            let text = msg.as_text()?.to_string();
                            Some(VectorSearchResult {
                                source_id: r.message_id.to_string(),
                                source_type: EmbeddingSourceType::Message,
                                channel_id: Some(r.conversation_id.to_string()),
                                content_preview: if text.len() > 200 {
                                    format!("{}...", &text[..197])
                                } else {
                                    text
                                },
                                score: 0.0, // replaced by RRF below
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };

        Ok(rrf_merge(vector_hits, keyword_hits, limit))
    }

    /// Index a single message: generate embedding and store.
    ///
    /// Skips if the message is already indexed.
    pub async fn index_message(
        &self,
        message_id: &str,
        channel_id: &str,
        content: &str,
    ) -> Result<bool, ServiceError> {
        // Skip if already indexed
        if self
            .vector_store
            .is_indexed(EmbeddingSourceType::Message, message_id)
            .map_err(ServiceError::Storage)?
        {
            return Ok(false);
        }

        // Skip empty/very short messages
        if content.trim().len() < 5 {
            return Ok(false);
        }

        let embedding = self
            .embedding_gen
            .embed_one(content)
            .await
            .map_err(backend_to_service)?;

        let preview = if content.len() > 200 {
            format!("{}...", &content[..197])
        } else {
            content.to_string()
        };

        let stored = StoredEmbedding {
            id: uuid::Uuid::new_v4().to_string(),
            source_type: EmbeddingSourceType::Message,
            source_id: message_id.to_string(),
            channel_id: Some(channel_id.to_string()),
            content_preview: preview,
            embedding,
            model: self.embedding_gen.model().to_string(),
            dimension: self.embedding_gen.dimension(),
        };

        self.vector_store
            .store(stored)
            .map_err(ServiceError::Storage)?;
        Ok(true)
    }

    /// Index multiple messages in batch.
    ///
    /// Returns the number of newly indexed messages.
    pub async fn index_messages_batch(
        &self,
        messages: &[(String, String, String)], // (message_id, channel_id, content)
    ) -> Result<u32, ServiceError> {
        // Filter out already-indexed and too-short messages
        let to_index: Vec<_> = messages
            .iter()
            .filter(|(id, _, content)| {
                content.trim().len() >= 5
                    && !self
                        .vector_store
                        .is_indexed(EmbeddingSourceType::Message, id)
                        .unwrap_or(true)
            })
            .collect();

        if to_index.is_empty() {
            return Ok(0);
        }

        // Generate embeddings in batch
        let texts: Vec<String> = to_index.iter().map(|(_, _, c)| c.clone()).collect();
        let embeddings = self
            .embedding_gen
            .embed_batch(&texts)
            .await
            .map_err(backend_to_service)?;

        // Build stored embeddings
        let stored: Vec<StoredEmbedding> = to_index
            .iter()
            .zip(embeddings.into_iter())
            .map(|((msg_id, ch_id, content), embedding)| {
                let preview = if content.len() > 200 {
                    format!("{}...", &content[..197])
                } else {
                    content.clone()
                };

                StoredEmbedding {
                    id: uuid::Uuid::new_v4().to_string(),
                    source_type: EmbeddingSourceType::Message,
                    source_id: msg_id.clone(),
                    channel_id: Some(ch_id.clone()),
                    content_preview: preview,
                    embedding,
                    model: self.embedding_gen.model().to_string(),
                    dimension: self.embedding_gen.dimension(),
                }
            })
            .collect();

        self.vector_store
            .store_batch(&stored)
            .map_err(ServiceError::Storage)
    }

    /// Delete the index for a channel.
    pub fn delete_channel_index(&self, channel_id: &str) -> Result<u32, ServiceError> {
        self.vector_store
            .delete_channel(channel_id)
            .map_err(ServiceError::Storage)
    }

    /// Get indexing statistics.
    pub fn stats(&self, channel_id: Option<&str>) -> Result<IndexStats, ServiceError> {
        let total = self
            .vector_store
            .count_total()
            .map_err(ServiceError::Storage)?;
        let channel_count = match channel_id {
            Some(id) => self
                .vector_store
                .count_channel(id)
                .map_err(ServiceError::Storage)?,
            None => 0,
        };

        Ok(IndexStats {
            total_indexed: total,
            channel_indexed: channel_count,
            model: self.embedding_gen.model().to_string(),
            dimension: self.embedding_gen.dimension(),
        })
    }
}

/// Indexing statistics.
#[derive(Debug)]
pub struct IndexStats {
    pub total_indexed: u32,
    pub channel_indexed: u32,
    pub model: String,
    pub dimension: usize,
}

fn backend_to_service(e: BackendError) -> ServiceError {
    ServiceError::Internal(format!("embedding generation failed: {}", e))
}

/// Reciprocal rank fusion over two ranked lists, deduped by source_id.
/// Standard k=60; score = Σ 1/(k + rank).
fn rrf_merge(
    a: Vec<VectorSearchResult>,
    b: Vec<VectorSearchResult>,
    limit: usize,
) -> Vec<VectorSearchResult> {
    const K: f32 = 60.0;
    let mut merged: Vec<(f32, VectorSearchResult)> = Vec::new();

    for list in [a, b] {
        for (rank, hit) in list.into_iter().enumerate() {
            let contribution = 1.0 / (K + rank as f32 + 1.0);
            if let Some(entry) = merged
                .iter_mut()
                .find(|(_, h)| h.source_id == hit.source_id)
            {
                entry.0 += contribution;
            } else {
                merged.push((contribution, hit));
            }
        }
    }

    merged.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    merged
        .into_iter()
        .take(limit)
        .map(|(score, mut hit)| {
            hit.score = score;
            hit
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str) -> VectorSearchResult {
        VectorSearchResult {
            source_id: id.to_string(),
            source_type: EmbeddingSourceType::Message,
            channel_id: None,
            content_preview: id.to_string(),
            score: 0.0,
        }
    }

    #[test]
    fn rrf_ranks_dual_leg_hits_first() {
        // "x" appears in both lists → must outrank single-leg items.
        let a = vec![hit("a1"), hit("x"), hit("a3")];
        let b = vec![hit("b1"), hit("x")];
        let merged = rrf_merge(a, b, 10);
        assert_eq!(merged[0].source_id, "x");
        assert_eq!(merged.len(), 4);
        assert!(merged[0].score > merged[1].score);
    }

    #[test]
    fn rrf_respects_limit_and_handles_empty_leg() {
        let a = vec![hit("1"), hit("2"), hit("3")];
        let merged = rrf_merge(a, Vec::new(), 2);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source_id, "1");
    }
}
