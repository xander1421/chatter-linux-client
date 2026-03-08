//! Jarvis Service - Main orchestrator for the AI assistant.
//!
//! Coordinates memory management, entity extraction, context building,
//! and LLM interaction to provide context-aware AI responses.

use std::sync::Arc;

use crate::domain::jarvis::{
    EntityKnowledge, JarvisContext, JarvisEntity, JarvisEntityId, JarvisEntityMention,
    JarvisFact, JarvisRelationship,
};
use crate::domain::UserId;
use crate::services::ai::{LocalAIActorHandle, SampleParams};
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use super::context_builder::ContextBuilder;
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
    /// Local AI actor handle.
    ai: LocalAIActorHandle,
    /// Entity extractor.
    extractor: EntityExtractor,
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
        ai: LocalAIActorHandle,
    ) -> Self {
        let extractor = EntityExtractor::new(ai.clone());
        let context_builder = ContextBuilder::new(memory_repo.clone(), graph_repo.clone());
        let memory_manager = MemoryManager::new(memory_repo.clone(), ai.clone());

        Self {
            memory_repo,
            graph_repo,
            ai,
            extractor,
            context_builder,
            memory_manager,
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
        // 1. Build enriched context
        let built_context = self.context_builder.build(context, query, author_id)?;

        // 2. Store the user's query
        self.memory_manager.store_user_message(context, query, author_id)?;

        // 3. Build the full prompt
        let system_context = built_context.to_system_prompt();
        let prompt = if system_context.is_empty() {
            format!("User: {}\n\nJarvis:", query)
        } else {
            format!(
                "You are Jarvis, a helpful AI assistant. Use the following context to inform your response.\n\n{}\n\n---\n\nUser: {}\n\nJarvis:",
                system_context, query
            )
        };

        // 4. Generate response
        let params = SampleParams {
            max_tokens: 512,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.1,
        };

        let response = self.ai.complete(prompt, params).await?;
        let response = response.trim().to_string();

        // 5. Store the response
        self.memory_manager.store_assistant_message(context, &response)?;

        // 6. Extract and store entities from the response (background)
        let _ = self.observe_message(context, &response, None, None).await;

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
        let entities = self.extractor.extract(message).await?;

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

    /// Get what Jarvis knows about an entity.
    pub fn get_entity_knowledge(&self, entity_name: &str) -> Result<EntityKnowledge, JarvisError> {
        self.graph_repo
            .get_entity_knowledge(entity_name, &self.memory_repo)
            .map_err(JarvisError::from)
    }

    /// Summarize a conversation.
    pub async fn summarize_conversation(
        &self,
        context: &JarvisContext,
    ) -> Result<String, JarvisError> {
        let conversations = self.memory_repo.get_recent_conversations(context, 50)?;

        if conversations.is_empty() {
            return Ok("No conversation history to summarize.".to_string());
        }

        let conversation_text: Vec<String> = conversations
            .iter()
            .map(|c| {
                let role = match c.role {
                    crate::domain::jarvis::Role::User => "User",
                    crate::domain::jarvis::Role::Assistant => "Jarvis",
                    crate::domain::jarvis::Role::System => "System",
                };
                format!("{}: {}", role, c.content)
            })
            .collect();

        let prompt = format!(
            r#"Summarize this conversation, highlighting:
1. Main topics discussed
2. Key decisions or conclusions
3. Action items or next steps

Conversation:
{}

Summary:"#,
            conversation_text.join("\n")
        );

        let params = SampleParams {
            max_tokens: 300,
            temperature: 0.3,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.0,
        };

        self.ai.complete(prompt, params).await.map_err(JarvisError::from)
    }

    // ========================================================================
    // Memory Management
    // ========================================================================

    /// Compress old conversations into summaries if needed.
    pub async fn compress_memory(
        &self,
        context: &JarvisContext,
    ) -> Result<bool, JarvisError> {
        let config = CompressionConfig::default();
        let summary = self.memory_manager.compress_if_needed(context, &config).await?;
        Ok(summary.is_some())
    }

    /// Apply decay to facts and prune low-confidence ones.
    pub fn apply_fact_decay(&self, threshold: f64) -> Result<u32, JarvisError> {
        self.memory_manager.apply_decay(threshold)
    }

    /// Get memory statistics for a context.
    pub fn get_memory_stats(&self, context: &JarvisContext) -> Result<MemoryStats, JarvisError> {
        self.memory_manager.get_stats(context)
    }

    // ========================================================================
    // Knowledge Graph
    // ========================================================================

    /// Search for entities by name.
    pub fn search_entities(&self, query: &str, limit: u32) -> Result<Vec<JarvisEntity>, JarvisError> {
        self.graph_repo
            .search_entities(query, limit)
            .map_err(JarvisError::from)
    }

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
            let mention = JarvisEntityMention::new(
                entity_id.clone(),
                context,
                message_id.map(String::from),
            );
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

        if lowercase.contains("i work on") || lowercase.contains("i'm working on") {
            if let Some(project) = Self::extract_after_phrase(message, &["work on", "working on"]) {
                let fact = JarvisFact::about_user(user_id, "works_on", &project)
                    .with_source(&context.context_id);
                self.memory_repo.upsert_fact(&fact)?;
            }
        }

        if lowercase.contains("i'm interested in") || lowercase.contains("i like") {
            if let Some(interest) = Self::extract_after_phrase(message, &["interested in", "i like"]) {
                let fact = JarvisFact::about_user(user_id, "interested_in", &interest)
                    .with_source(&context.context_id);
                self.memory_repo.upsert_fact(&fact)?;
            }
        }

        if lowercase.contains("i use") || lowercase.contains("i prefer") {
            if let Some(tool) = Self::extract_after_phrase(message, &["i use", "i prefer"]) {
                let fact = JarvisFact::about_user(user_id, "uses", &tool)
                    .with_source(&context.context_id);
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
                let extracted = rest
                    .split(['.', ',', '!', '?'])
                    .next()?
                    .trim();

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
