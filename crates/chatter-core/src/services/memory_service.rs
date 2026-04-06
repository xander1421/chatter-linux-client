//! Memory Service - Bridge between server memory service and local AI extraction.
//!
//! Provides a unified API for AI memory operations, handling both:
//! 1. Server-side processing for non-E2E channels (direct gRPC calls)
//! 2. Client-side extraction for E2E channels (local Ollama → Remember RPC)
//!
//! SECURITY: E2E encrypted content is NEVER sent to the server for processing.
//! Client decrypts locally, runs extraction via local AI, then sends only the
//! extracted memories (not plaintext) to the server for storage.

use std::sync::Arc;

use crate::error::NetworkError;
use crate::network::grpc::GrpcClientPool;
use crate::proto::chat_v1::{
    BuildContextResponse, ConversationEpisode, GetMemoryStatsResponse,
    GetUserMemoryProfileResponse, MemoryImportance, MemoryType, ProcessingSource,
    RememberResponse, SearchResponse, SummarizeConversationResponse,
};
use crate::services::ai::{LocalAIActorHandle, SampleParams};

/// Memory service error types.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("Network error: {0}")]
    Network(#[from] NetworkError),

    #[error("Local AI not available for client-side extraction")]
    LocalAIUnavailable,

    #[error("Local AI extraction failed: {0}")]
    ExtractionFailed(String),

    #[error("E2E channel content must not be sent to server for processing")]
    E2EViolation,
}

/// Memory service bridging server memory RPCs with local AI extraction.
///
/// For non-E2E channels, calls the server memory service directly.
/// For E2E channels, uses local AI to extract memories client-side,
/// then stores them via the Remember RPC with PROCESSING_SOURCE_CLIENT.
pub struct MemoryService {
    grpc_pool: Arc<GrpcClientPool>,
    local_ai: LocalAIActorHandle,
}

impl MemoryService {
    /// Create a new MemoryService.
    pub fn new(grpc_pool: Arc<GrpcClientPool>, local_ai: LocalAIActorHandle) -> Self {
        Self { grpc_pool, local_ai }
    }

    // ==================== Direct Server RPCs ====================

    /// Store a memory explicitly (user-initiated or extraction result).
    pub async fn remember(
        &self,
        content: &str,
        memory_type: MemoryType,
        channel_id: Option<&str>,
        importance: MemoryImportance,
        source: ProcessingSource,
        category: &str,
    ) -> Result<RememberResponse, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        client
            .remember(
                content,
                memory_type,
                channel_id,
                importance,
                source,
                category,
                std::collections::HashMap::new(),
            )
            .await
            .map_err(MemoryError::Network)
    }

    /// Semantic search over memories.
    pub async fn search(
        &self,
        query: &str,
        channel_id: Option<&str>,
        limit: u32,
    ) -> Result<SearchResponse, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        client
            .search(query, channel_id, limit, true, false)
            .await
            .map_err(MemoryError::Network)
    }

    /// Build context for LLM prompts within a token budget.
    pub async fn build_context(
        &self,
        query: &str,
        channel_id: Option<&str>,
        token_budget: u32,
    ) -> Result<BuildContextResponse, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        client
            .build_context(query, channel_id, token_budget)
            .await
            .map_err(MemoryError::Network)
    }

    /// Get the user's memory profile.
    pub async fn get_profile(&self) -> Result<GetUserMemoryProfileResponse, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        client
            .get_user_memory_profile()
            .await
            .map_err(MemoryError::Network)
    }

    /// Get memory statistics.
    pub async fn get_stats(&self) -> Result<GetMemoryStatsResponse, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        client
            .get_memory_stats()
            .await
            .map_err(MemoryError::Network)
    }

    /// Delete a specific memory.
    pub async fn forget(&self, memory_id: &str) -> Result<bool, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        let resp = client.forget(memory_id).await.map_err(MemoryError::Network)?;
        Ok(resp.found)
    }

    /// Summarize a conversation (non-E2E channels only).
    ///
    /// SECURITY: Server rejects E2E channels with PERMISSION_DENIED.
    pub async fn summarize_conversation(
        &self,
        channel_id: &str,
        episodes: Vec<ConversationEpisode>,
    ) -> Result<SummarizeConversationResponse, MemoryError> {
        let mut client = self.grpc_pool.memory_client();
        client
            .summarize_conversation(channel_id, episodes)
            .await
            .map_err(MemoryError::Network)
    }

    // ==================== Client-Side Extraction (E2E Safe) ====================

    /// Extract and store memories from E2E encrypted conversation content.
    ///
    /// This is the E2E-safe extraction pipeline:
    /// 1. Client has already decrypted the messages locally
    /// 2. Local AI extracts memories from plaintext
    /// 3. Only extracted memories (not plaintext) are sent to the server
    ///
    /// The `source` is always set to PROCESSING_SOURCE_CLIENT.
    pub async fn extract_and_store_e2e(
        &self,
        channel_id: &str,
        decrypted_messages: &[String],
    ) -> Result<Vec<RememberResponse>, MemoryError> {
        if decrypted_messages.is_empty() {
            return Ok(vec![]);
        }

        // Build extraction prompt for local AI
        let conversation = decrypted_messages.join("\n");
        let prompt = format!(
            "Extract key facts, preferences, and important information from this conversation. \
             For each extracted memory, output one line in the format:\n\
             TYPE|IMPORTANCE|CATEGORY|CONTENT\n\
             Where TYPE is one of: fact, preference, episode, personal, goal\n\
             Where IMPORTANCE is one of: low, medium, high, critical\n\
             Where CATEGORY is a short label (e.g., work, hobby, family)\n\n\
             Conversation:\n{}\n\n\
             Extracted memories:",
            conversation
        );

        let params = SampleParams {
            max_tokens: 500,
            ..Default::default()
        };

        let extraction = self
            .local_ai
            .complete(prompt, params)
            .await
            .map_err(|e| MemoryError::ExtractionFailed(e.to_string()))?;

        // Parse extraction output and store each memory
        let mut results = Vec::new();
        let mut client = self.grpc_pool.memory_client();

        for line in extraction.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() < 4 {
                continue;
            }

            let memory_type = match parts[0].trim().to_lowercase().as_str() {
                "preference" => MemoryType::Preference,
                "event" | "episode" => MemoryType::Episode,
                "personal" => MemoryType::Personal,
                "goal" => MemoryType::Goal,
                _ => MemoryType::Fact,
            };

            let importance = match parts[1].trim().to_lowercase().as_str() {
                "high" => MemoryImportance::High,
                "critical" => MemoryImportance::Critical,
                "low" => MemoryImportance::Low,
                _ => MemoryImportance::Medium,
            };

            let category = parts[2].trim();
            let content = parts[3].trim();

            if content.is_empty() {
                continue;
            }

            match client
                .remember(
                    content,
                    memory_type,
                    Some(channel_id),
                    importance,
                    ProcessingSource::Client,
                    category,
                    std::collections::HashMap::new(),
                )
                .await
            {
                Ok(resp) => results.push(resp),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        content = content,
                        "Failed to store extracted memory, continuing with remaining"
                    );
                }
            }
        }

        Ok(results)
    }
}
