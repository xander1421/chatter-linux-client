//! Memory gRPC client.
//!
//! Client for AI memory service operations including storing/retrieving memories,
//! semantic search, knowledge graph traversal, and context building for LLM prompts.
//!
//! SECURITY: ProcessEpisode and SummarizeConversation must NOT be called with
//! E2E encrypted content. For E2E channels, use client-side extraction with
//! local Ollama and call Remember with PROCESSING_SOURCE_CLIENT.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    memory_service_client::MemoryServiceClient as ProtoMemoryServiceClient,
    BatchRememberRequest, BatchRememberResponse, BuildContextRequest, BuildContextResponse,
    ConversationEpisode, DecayMemoriesRequest, DecayMemoriesResponse, ForgetRequest,
    ForgetResponse, GetEntityRequest, GetEntityResponse, GetMemoryStatsRequest,
    GetMemoryStatsResponse, GetRelatedEntitiesRequest, GetRelatedEntitiesResponse,
    GetUserMemoryProfileRequest, GetUserMemoryProfileResponse, MemoryImportance, MemoryType,
    ProcessingSource, RememberRequest, RememberResponse, SearchRequest, SearchResponse,
    SummarizeConversationRequest, SummarizeConversationResponse,
};

/// Memory service client wrapper.
///
/// Wraps the generated MemoryServiceClient with ergonomic APIs and error mapping.
pub struct MemoryClient {
    inner: ProtoMemoryServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl MemoryClient {
    /// Create a new MemoryClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: ProtoMemoryServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Memory Storage RPCs ====================

    /// Store a memory (explicit user request or client-side extraction result).
    ///
    /// For E2E channels, the client decrypts locally, extracts via local Ollama,
    /// then calls this with `source` set to `PROCESSING_SOURCE_CLIENT`.
    pub async fn remember(
        &mut self,
        content: &str,
        memory_type: MemoryType,
        channel_id: Option<&str>,
        importance: MemoryImportance,
        source: ProcessingSource,
        category: &str,
        metadata: std::collections::HashMap<String, String>,
    ) -> Result<RememberResponse, NetworkError> {
        let request = RememberRequest {
            content: content.to_string(),
            r#type: memory_type as i32,
            channel_id: channel_id.map(|s| s.to_string()),
            importance: importance as i32,
            sentiment: 0, // UNSPECIFIED
            source: source as i32,
            category: category.to_string(),
            metadata,
            request_id: String::new(),
        };

        self.inner
            .remember(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Store multiple memories at once (client-side batch extraction).
    pub async fn batch_remember(
        &mut self,
        memories: Vec<RememberRequest>,
    ) -> Result<BatchRememberResponse, NetworkError> {
        let request = BatchRememberRequest { memories };

        self.inner
            .batch_remember(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete a specific memory.
    pub async fn forget(&mut self, memory_id: &str) -> Result<ForgetResponse, NetworkError> {
        let request = ForgetRequest {
            memory_id: memory_id.to_string(),
            request_id: String::new(),
        };

        self.inner
            .forget(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Search & Context RPCs ====================

    /// Semantic search over memories with optional entity/relationship inclusion.
    pub async fn search(
        &mut self,
        query: &str,
        channel_id: Option<&str>,
        limit: u32,
        include_entities: bool,
        include_relationships: bool,
    ) -> Result<SearchResponse, NetworkError> {
        let request = SearchRequest {
            query: query.to_string(),
            channel_id: channel_id.map(|s| s.to_string()),
            limit,
            min_score: 0.0,
            include_entities,
            include_relationships,
            memory_types: vec![],
        };

        self.inner
            .search(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Build a formatted context string for LLM prompts within a token budget.
    pub async fn build_context(
        &mut self,
        query: &str,
        channel_id: Option<&str>,
        token_budget: u32,
    ) -> Result<BuildContextResponse, NetworkError> {
        let request = BuildContextRequest {
            query: query.to_string(),
            channel_id: channel_id.map(|s| s.to_string()),
            token_budget,
        };

        self.inner
            .build_context(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Knowledge Graph RPCs ====================

    /// Get a specific entity from the knowledge graph.
    pub async fn get_entity(
        &mut self,
        entity_id: &str,
    ) -> Result<GetEntityResponse, NetworkError> {
        let request = GetEntityRequest {
            entity_id: entity_id.to_string(),
        };

        self.inner
            .get_entity(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get entities related to a given entity via graph traversal.
    pub async fn get_related_entities(
        &mut self,
        entity_id: &str,
        depth: u32,
        relation_types: Vec<i32>,
    ) -> Result<GetRelatedEntitiesResponse, NetworkError> {
        let request = GetRelatedEntitiesRequest {
            entity_id: entity_id.to_string(),
            depth,
            relation_types,
        };

        self.inner
            .get_related_entities(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Profile & Stats RPCs ====================

    /// Get the AI's aggregated understanding of the authenticated user.
    pub async fn get_user_memory_profile(
        &mut self,
    ) -> Result<GetUserMemoryProfileResponse, NetworkError> {
        self.inner
            .get_user_memory_profile(GetUserMemoryProfileRequest {})
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get memory statistics for the authenticated user.
    pub async fn get_memory_stats(
        &mut self,
    ) -> Result<GetMemoryStatsResponse, NetworkError> {
        self.inner
            .get_memory_stats(GetMemoryStatsRequest {})
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Trigger time-based memory decay for the authenticated user.
    pub async fn decay_memories(
        &mut self,
    ) -> Result<DecayMemoriesResponse, NetworkError> {
        self.inner
            .decay_memories(DecayMemoriesRequest {})
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Server-Side Processing RPCs ====================
    // NOTE: These must NOT be called with E2E encrypted content.

    /// Summarize a set of conversation episodes.
    ///
    /// SECURITY: Must NOT be called with E2E encrypted channel content.
    /// Server returns PERMISSION_DENIED for E2E channels.
    pub async fn summarize_conversation(
        &mut self,
        channel_id: &str,
        episodes: Vec<ConversationEpisode>,
    ) -> Result<SummarizeConversationResponse, NetworkError> {
        let request = SummarizeConversationRequest {
            channel_id: channel_id.to_string(),
            episodes,
        };

        self.inner
            .summarize_conversation(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_client_construction_types() {
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = MemoryClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_remember_request_construction() {
        let request = RememberRequest {
            content: "User prefers dark mode".to_string(),
            r#type: MemoryType::Preference as i32,
            channel_id: Some("channel-123".to_string()),
            importance: MemoryImportance::High as i32,
            sentiment: 0,
            source: ProcessingSource::Client as i32,
            category: "ui_preference".to_string(),
            metadata: std::collections::HashMap::new(),
            request_id: String::new(),
        };

        assert_eq!(request.content, "User prefers dark mode");
        assert_eq!(request.r#type, MemoryType::Preference as i32);
        assert_eq!(request.source, ProcessingSource::Client as i32);
    }

    #[test]
    fn test_search_request_construction() {
        let request = SearchRequest {
            query: "dark mode preferences".to_string(),
            channel_id: None,
            limit: 10,
            min_score: 0.0,
            include_entities: true,
            include_relationships: false,
            memory_types: vec![],
        };

        assert_eq!(request.query, "dark mode preferences");
        assert_eq!(request.limit, 10);
        assert!(request.include_entities);
    }

    #[test]
    fn test_processing_source_values() {
        assert_eq!(ProcessingSource::Unspecified as i32, 0);
        assert_eq!(ProcessingSource::Server as i32, 1);
        assert_eq!(ProcessingSource::Client as i32, 2);
        assert_eq!(ProcessingSource::Explicit as i32, 3);
    }
}
