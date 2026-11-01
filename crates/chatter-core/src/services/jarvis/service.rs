//! Jarvis Service - Main orchestrator for the AI assistant.
//!
//! Coordinates memory management, entity extraction, context building,
//! and LLM interaction to provide context-aware AI responses.

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::jarvis::{
    EntityKnowledge, JarvisContext, JarvisEntity, JarvisEntityId, JarvisEntityMention, JarvisFact,
    JarvisRelationship,
};
use crate::services::ai::SampleParams;
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use super::context_builder::ContextBuilder;
use super::engine::SharedJarvisEngine;
use super::entity_extractor::{EntityExtractor, ExtractedEntity};
use super::error::JarvisError;
use super::memory_manager::{CompressionConfig, MemoryManager, MemoryStats};

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
    /// Entity extractor.
    extractor: EntityExtractor,
    /// Context builder.
    context_builder: ContextBuilder,
    /// Memory manager.
    memory_manager: MemoryManager,
    /// One compression at a time; a turn that finds it held skips.
    compress_lock: tokio::sync::Mutex<()>,
}

impl JarvisService {
    /// Create a new Jarvis service.
    pub fn new(
        memory_repo: Arc<JarvisMemoryRepository>,
        graph_repo: Arc<JarvisGraphRepository>,
        ai: SharedJarvisEngine,
    ) -> Self {
        let extractor = EntityExtractor::new(ai.clone());
        let context_builder = ContextBuilder::new(memory_repo.clone(), graph_repo.clone());
        let memory_manager =
            MemoryManager::new(memory_repo.clone(), graph_repo.clone(), ai.clone());

        Self {
            memory_repo,
            graph_repo,
            ai,
            extractor,
            context_builder,
            memory_manager,
            compress_lock: tokio::sync::Mutex::new(()),
        }
    }

    // ========================================================================
    // Main API
    // ========================================================================

    /// Ask Jarvis a question with full context.
    ///
    /// Retrieves relevant context from memory and the knowledge graph,
    /// generates a response, and stores the interaction in memory.
    pub async fn ask(
        &self,
        context: &JarvisContext,
        query: &str,
        author_id: Option<&UserId>,
    ) -> Result<String, JarvisError> {
        self.ask_in(
            context,
            query,
            author_id,
            crate::ai_policy::policy::Silo::Local,
        )
        .await
    }

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

    /// Observe a message without generating a response.
    ///
    /// Extracts entities and learns facts from the message. Use this for
    /// passive learning from channel messages.
    pub async fn observe_message(
        &self,
        context: &JarvisContext,
        message: &str,
        author_id: Option<&UserId>,
        message_id: Option<&str>,
    ) -> Result<(), JarvisError> {
        // 1. Extract entities
        let access = crate::ai_policy::policy::ai_access(&context.conversation_type(), false);
        let entities = self.extractor.extract(access, message).await?;

        // 2. Store entities in the knowledge graph
        let entity_ids = self.store_entities(&entities, context, message_id)?;

        // 3. Create relationships between co-mentioned entities
        self.create_co_mention_relationships(&entity_ids)?;

        // 4. Extract and store facts
        if let Some(user_id) = author_id {
            self.extract_user_facts(user_id, message, context)?;
        }

        Ok(())
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

    /// Get what Jarvis knows about an entity.
    pub fn get_entity_knowledge(&self, entity_name: &str) -> Result<EntityKnowledge, JarvisError> {
        self.graph_repo
            .get_entity_knowledge(entity_name, &self.memory_repo)
            .map_err(JarvisError::from)
    }

    /// Summarize only the owner's explicitly remote conversation history.
    pub async fn summarize_conversation(
        &self,
        context: &JarvisContext,
        owner: &UserId,
    ) -> Result<String, JarvisError> {
        self.ask_in(context, "Summarize this remote conversation, highlighting main topics, decisions and next steps.", Some(owner), crate::ai_policy::policy::Silo::Cloud).await
    }

    // ========================================================================
    // Memory Management
    // ========================================================================

    /// Compress old conversations into a summary once the context's stored
    /// history passes the builder's window. Returns whether a summary was
    /// written. A compression already running for any context makes this a
    /// no-op rather than a second LLM call over the same rows.
    pub async fn compress_memory(&self, context: &JarvisContext) -> Result<bool, JarvisError> {
        let Ok(_guard) = self.compress_lock.try_lock() else {
            return Ok(false);
        };
        let summary = self
            .memory_manager
            .compress_if_needed(context, &CompressionConfig::default())
            .await?;
        Ok(summary.is_some())
    }

    /// Get memory statistics for a context.
    pub fn get_memory_stats(&self, context: &JarvisContext) -> Result<MemoryStats, JarvisError> {
        self.memory_manager.get_stats(context)
    }

    /// Delete a context's conversation history and summaries. Returns the
    /// number of turns deleted.
    pub fn clear_memory(&self, context: &JarvisContext) -> Result<u32, JarvisError> {
        self.memory_manager.clear_context(context)
    }

    /// Apply decay to facts and prune low-confidence ones.
    pub fn apply_fact_decay(&self, threshold: f64) -> Result<u32, JarvisError> {
        self.memory_manager.apply_decay(threshold)
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

    /// Prune old data from the knowledge graph.
    pub fn prune_graph(&self, days_old: u32) -> Result<(u32, u32), JarvisError> {
        self.graph_repo
            .prune_old_data(days_old)
            .map_err(JarvisError::from)
    }

    // ========================================================================
    // Private Helpers
    // ========================================================================

    /// Store extracted entities in the graph.
    fn store_entities(
        &self,
        entities: &[ExtractedEntity],
        context: &JarvisContext,
        message_id: Option<&str>,
    ) -> Result<Vec<JarvisEntityId>, JarvisError> {
        let mut entity_ids = Vec::new();

        for extracted in entities {
            let entity = extracted.to_jarvis_entity();
            let entity_id = self.graph_repo.upsert_entity(&entity)?;

            // Record the mention
            let mention =
                JarvisEntityMention::new(entity_id.clone(), context, message_id.map(String::from));
            self.graph_repo.insert_mention(&mention)?;

            entity_ids.push(entity_id);
        }

        Ok(entity_ids)
    }

    /// Create "mentioned_with" relationships between co-mentioned entities.
    fn create_co_mention_relationships(
        &self,
        entity_ids: &[JarvisEntityId],
    ) -> Result<(), JarvisError> {
        // Create relationships between all pairs of co-mentioned entities
        for i in 0..entity_ids.len() {
            for j in (i + 1)..entity_ids.len() {
                let rel = JarvisRelationship::new(
                    entity_ids[i].clone(),
                    entity_ids[j].clone(),
                    "mentioned_with".to_string(),
                );
                self.graph_repo.upsert_relationship(&rel)?;
            }
        }

        Ok(())
    }

    /// Extract facts about a user from a message.
    fn extract_user_facts(
        &self,
        user_id: &UserId,
        message: &str,
        context: &JarvisContext,
    ) -> Result<(), JarvisError> {
        // Simple heuristic fact extraction
        // In a production system, this would use the LLM

        // Look for "I work on/at/with X" patterns
        let lowercase = message.to_lowercase();

        if lowercase.contains("my name is") || lowercase.contains("call me") {
            if let Some(name) = Self::extract_after_phrase(message, &["my name is", "call me"]) {
                let fact = JarvisFact::about_user(user_id, "is_named", &name)
                    .with_source(&context.context_id);
                self.memory_repo.upsert_fact(&fact)?;
            }
        }

        if lowercase.contains("i work on") || lowercase.contains("i'm working on") {
            if let Some(project) = Self::extract_after_phrase(message, &["work on", "working on"]) {
                let fact = JarvisFact::about_user(user_id, "works_on", &project)
                    .with_source(&context.context_id);
                self.memory_repo.upsert_fact(&fact)?;
            }
        }

        if lowercase.contains("i'm interested in") || lowercase.contains("i like") {
            if let Some(interest) =
                Self::extract_after_phrase(message, &["interested in", "i like"])
            {
                let fact = JarvisFact::about_user(user_id, "interested_in", &interest)
                    .with_source(&context.context_id);
                self.memory_repo.upsert_fact(&fact)?;
            }
        }

        if lowercase.contains("i use") || lowercase.contains("i prefer") {
            if let Some(tool) = Self::extract_after_phrase(message, &["i use", "i prefer"]) {
                let fact =
                    JarvisFact::about_user(user_id, "uses", &tool).with_source(&context.context_id);
                self.memory_repo.upsert_fact(&fact)?;
            }
        }

        Ok(())
    }

    /// Extract text after a phrase (simple extraction).
    fn extract_after_phrase(text: &str, phrases: &[&str]) -> Option<String> {
        let lowercase = text.to_lowercase();

        for phrase in phrases {
            if let Some(pos) = lowercase.find(phrase) {
                let start = pos + phrase.len();
                let rest = &text[start..];

                // Extract until end of sentence or specific delimiter
                let extracted = rest.split(['.', ',', '!', '?']).next()?.trim();

                if !extracted.is_empty() && extracted.len() < 100 {
                    return Some(extracted.to_string());
                }
            }
        }

        None
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
        assert!(matches!(service.summarize_conversation(&private, &owner).await, Err(JarvisError::NotPermitted(_))));
        for context in [general, private] {
            assert_eq!(memory.count_conversations(&context).unwrap(), 0);
            assert_eq!(memory.count_conversations(&context.for_remote(&owner)).unwrap(), 0);
        }
    }

    #[test]
    fn test_extract_after_phrase() {
        let text = "I work on the Chatter project.";
        let result = JarvisService::extract_after_phrase(text, &["work on"]);
        assert_eq!(result, Some("the Chatter project".to_string()));
    }

    #[test]
    fn test_extract_after_phrase_with_comma() {
        let text = "I like Rust, Python, and Go";
        let result = JarvisService::extract_after_phrase(text, &["i like"]);
        assert_eq!(result, Some("Rust".to_string()));
    }

    #[test]
    fn test_extract_after_phrase_not_found() {
        let text = "Hello world";
        let result = JarvisService::extract_after_phrase(text, &["work on"]);
        assert!(result.is_none());
    }
}
