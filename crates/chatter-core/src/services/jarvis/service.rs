//! Jarvis Service - Main orchestrator for the AI assistant.
//!
//! Coordinates memory management, entity extraction, context building,
//! and LLM interaction to provide context-aware AI responses.

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::jarvis::{JarvisContext, JarvisEntity, JarvisEntityId};
use crate::services::ai::SampleParams;
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use super::context_builder::ContextBuilder;
use super::engine::SharedJarvisEngine;
use super::error::JarvisError;
use super::memory_manager::{MemoryManager, MemoryStats};

/// Jarvis AI assistant service.
///
/// Provides a persistent AI assistant with memory and knowledge graph
/// capabilities for context-aware responses.
pub struct JarvisService {
    /// Memory repository.
    memory_repo: Arc<JarvisMemoryRepository>,
    /// Graph repository.
    graph_repo: Arc<JarvisGraphRepository>,
    /// Inference engine (active provider or in-process actor).
    ai: SharedJarvisEngine,
    /// Context builder.
    context_builder: ContextBuilder,
    /// Memory manager.
    memory_manager: MemoryManager,
}

impl JarvisService {
    /// Create a new Jarvis service.
    pub fn new(
        memory_repo: Arc<JarvisMemoryRepository>,
        graph_repo: Arc<JarvisGraphRepository>,
        ai: SharedJarvisEngine,
    ) -> Self {
        let context_builder = ContextBuilder::new(memory_repo.clone());
        let memory_manager =
            MemoryManager::new(memory_repo.clone(), graph_repo.clone());

        Self {
            memory_repo,
            graph_repo,
            ai,
            context_builder,
            memory_manager,
        }
    }

    // ========================================================================
    // Main API
    // ========================================================================

    /// Ask, running in an explicitly chosen silo.
    ///
    /// The caller resolves the silo from the conversation; policy is still
    /// enforced inside the engine, so a Cloud request on a DM is refused here
    /// rather than honoured.
    pub async fn ask_in(
        &self,
        context: &JarvisContext,
        query: &str,
        author_id: Option<&UserId>,
        silo: crate::ai_policy::policy::Silo,
    ) -> Result<String, JarvisError> {
        use crate::ai_policy::policy::{Silo, ai_access};
        use crate::ai_protocol::{ChatMessage, ChatRole};
        let access = ai_access(&context.conversation_type(), false);
        if !access.permits_silo(silo) {
            return Err(JarvisError::NotPermitted(
                "Remote AI is not permitted in this conversation".into(),
            ));
        }
        if silo != Silo::Cloud || !self.ai.has_cloud() {
            return Err(JarvisError::NoModel);
        }
        let owner = author_id
            .ok_or_else(|| JarvisError::NotPermitted("Sign in before using remote AI".into()))?;
        let remote = context.for_remote(owner);
        let built = self.context_builder.build_remote(context, owner)?;
        let mut messages = Vec::with_capacity(built.history.len() + 2);
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: "You are the user's remote AI assistant. Answer the request directly using this remote conversation. Do not claim to run on the device or to remember facts outside this conversation.".into(),
        });
        for turn in &built.history {
            messages.push(ChatMessage {
                role: match turn.role {
                    crate::domain::jarvis::Role::User => ChatRole::User,
                    crate::domain::jarvis::Role::Assistant => ChatRole::Assistant,
                    crate::domain::jarvis::Role::System => ChatRole::System,
                },
                content: turn.content.clone(),
            });
        }
        messages.push(ChatMessage {
            role: ChatRole::User,
            content: query.to_string(),
        });
        self.memory_manager
            .store_user_message(&remote, query, Some(owner))?;
        let params = SampleParams {
            max_tokens: 1024,
            ..Default::default()
        };
        let response = self
            .ai
            .complete_conversation_in(access, silo, messages, params)
            .await?;
        let response = response.trim().to_string();
        self.memory_manager
            .store_assistant_message(&remote, &response)?;
        Ok(response)
    }

    /// All facts about a user, superseded history included (memory browser).
    pub fn list_user_facts(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<crate::domain::jarvis::JarvisFact>, JarvisError> {
        self.memory_repo
            .list_facts_for_subject(
                crate::domain::jarvis::SubjectType::User,
                &user_id.to_string(),
            )
            .map_err(JarvisError::from)
    }

    /// Get persisted conversation history for a context (chronological).
    pub fn get_conversation_history(
        &self,
        context: &JarvisContext,
        limit: u32,
    ) -> Result<Vec<crate::domain::jarvis::JarvisConversation>, JarvisError> {
        self.memory_repo
            .get_recent_conversations(context, limit)
            .map_err(JarvisError::from)
    }

    // ========================================================================
    // Memory Management
    // ========================================================================

    /// Get memory statistics for a context.
    pub fn get_memory_stats(&self, context: &JarvisContext) -> Result<MemoryStats, JarvisError> {
        self.memory_manager.get_stats(context)
    }

    /// Delete a context's conversation history and summaries. Returns the
    /// number of turns deleted.
    pub fn clear_memory(&self, context: &JarvisContext) -> Result<u32, JarvisError> {
        self.memory_manager.clear_context(context)
    }

    // ========================================================================
    // Knowledge Graph
    // ========================================================================

    /// Get the top mentioned entities.
    pub fn get_top_entities(&self, limit: u32) -> Result<Vec<JarvisEntity>, JarvisError> {
        self.graph_repo
            .get_top_entities(limit)
            .map_err(JarvisError::from)
    }

    /// Get entities connected to a given entity.
    pub fn get_connected_entities(
        &self,
        entity_id: &JarvisEntityId,
        max_depth: u32,
    ) -> Result<Vec<(JarvisEntity, u32)>, JarvisError> {
        self.graph_repo
            .get_connected_entities(entity_id, max_depth)
            .map_err(JarvisError::from)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_or_private_remote_requests_do_not_write_history() {
        use crate::services::jarvis::engine::JarvisEngine;
        use crate::storage::Database;
        use crate::ai_policy::policy::Silo;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("remote.db"), "fixture-pass").unwrap());
        db.run_migrations().unwrap();
        let memory = Arc::new(JarvisMemoryRepository::new(db.clone()));
        let service = JarvisService::new(memory.clone(), Arc::new(JarvisGraphRepository::new(db)), SharedJarvisEngine::new(JarvisEngine::Disabled));
        let owner = UserId::new();
        let general = JarvisContext::general();
        assert!(matches!(service.ask_in(&general, "fixture", Some(&owner), Silo::Cloud).await, Err(JarvisError::NoModel)));
        let private = JarvisContext::dm(crate::domain::ChannelId::new());
        assert!(matches!(service.ask_in(&private, "fixture", Some(&owner), Silo::Cloud).await, Err(JarvisError::NotPermitted(_))));
        for context in [general, private] {
            assert_eq!(memory.count_conversations(&context).unwrap(), 0);
            assert_eq!(memory.count_conversations(&context.for_remote(&owner)).unwrap(), 0);
        }
    }
}
