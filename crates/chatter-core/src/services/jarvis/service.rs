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
use crate::services::ai::SampleParams;
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;

use super::context_builder::ContextBuilder;
use super::engine::SharedJarvisEngine;
use super::entity_extractor::{EntityExtractor, ExtractedEntity};
use super::fact_extractor::FactExtractor;
use super::error::JarvisError;
use super::memory_manager::{CompressionConfig, MemoryManager, MemoryStats};

/// Supplies live app state (channel names, friends) for the system prompt.
/// A closure so Jarvis needs no dependency on channel/user services.
pub type AppContextFn = Arc<dyn Fn() -> String + Send + Sync>;

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
    /// Event bus for emitting events.
    event_bus: EventBus,
    /// Entity extractor.
    extractor: EntityExtractor,
    /// LLM fact extractor (background write path).
    fact_extractor: Arc<FactExtractor>,
    /// Context builder.
    context_builder: ContextBuilder,
    /// Memory manager.
    memory_manager: MemoryManager,
    /// App-context bridge (channels, friends) for grounded answers.
    app_context: Option<AppContextFn>,
}

impl JarvisService {
    /// Create a new Jarvis service.
    pub fn new(
        memory_repo: Arc<JarvisMemoryRepository>,
        graph_repo: Arc<JarvisGraphRepository>,
        ai: SharedJarvisEngine,
        event_bus: EventBus,
        semantic: Option<Arc<crate::services::semantic_search::SemanticSearchService>>,
    ) -> Self {
        let extractor = EntityExtractor::new(ai.clone());
        let fact_extractor = Arc::new(FactExtractor::new(ai.clone(), memory_repo.clone()));
        let context_builder = ContextBuilder::new(memory_repo.clone(), graph_repo.clone(), semantic);
        let memory_manager = MemoryManager::new(memory_repo.clone(), graph_repo.clone(), ai.clone());

        Self {
            memory_repo,
            graph_repo,
            ai,
            event_bus,
            extractor,
            fact_extractor,
            context_builder,
            memory_manager,
            app_context: None,
        }
    }

    /// Attach the app-context bridge (builder style).
    pub fn with_app_context(mut self, f: AppContextFn) -> Self {
        self.app_context = Some(f);
        self
    }

    // ========================================================================
    // Main API
    // ========================================================================

    /// Process user input to detect and handle Jarvis commands.
    ///
    /// Returns true if the input was handled as a Jarvis command, false otherwise.
    /// This allows the UI to simply check if Jarvis handled the input.
    ///
    /// Handles:
    /// - `/ask <question>` - Direct Jarvis query
    /// - `@Jarvis <message>` or `@jarvis <message>` - Mention-based query
    /// - `/jarvis <command>` - Various Jarvis commands (stats, clear, etc.)
    pub async fn process_input(
        &self,
        text: &str,
        context: &JarvisContext,
        author_id: Option<&UserId>,
    ) -> Result<bool, JarvisError> {
        // Check for /ask command
        if let Some(query) = text.strip_prefix("/ask ") {
            let response = self.ask(context, query, author_id).await?;
            
            // Emit event for UI to display response
            self.event_bus.emit_domain(DomainEvent::JarvisResponse {
                text: response,
                context_id: context.context_id.clone(),
            });
            
            tracing::info!("Jarvis handled /ask command");
            return Ok(true);
        }

        // Check for @Jarvis or @jarvis mention
        if text.starts_with("@Jarvis ") || text.starts_with("@jarvis ") {
            let query = text.trim_start_matches("@Jarvis ")
                .trim_start_matches("@jarvis ")
                .trim();
            if !query.is_empty() {
                let response = self.ask(context, query, author_id).await?;
                
                // Emit event for UI to display response
                self.event_bus.emit_domain(DomainEvent::JarvisResponse {
                    text: response,
                    context_id: context.context_id.clone(),
                });
                
                tracing::info!("Jarvis handled @mention");
                return Ok(true);
            }
        }

        // Check for /jarvis commands
        if text.starts_with("/jarvis") {
            if let Some(cmd) = text.strip_prefix("/jarvis ") {
                match cmd {
                    "stats" => {
                        let stats = self.memory_manager.get_stats(context)?;
                        
                        // Emit JarvisStats event
                        self.event_bus.emit_domain(DomainEvent::JarvisStats {
                            stats: format!(
                                "Memory Stats:\n- Conversations: {}\n- Facts: {}\n- Entities: {}",
                                stats.conversation_count,
                                stats.fact_count,
                                stats.entity_count
                            ),
                        });
                        
                        tracing::info!("Jarvis handled stats command");
                        return Ok(true);
                    }
                    "clear" => {
                        self.memory_manager.clear_context(context)?;
                        
                        // Emit JarvisCleared event
                        self.event_bus.emit_domain(DomainEvent::JarvisCleared {
                            context_id: context.context_id.clone(),
                        });
                        
                        tracing::info!("Jarvis handled clear command");
                        return Ok(true);
                    }
                    _ => {
                        // Unknown jarvis command, let it through as regular message
                        return Ok(false);
                    }
                }
            }
        }

        // Not a Jarvis command
        Ok(false)
    }

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
        self.ask_in(context, query, author_id, crate::ai_policy::policy::Silo::Local)
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
        // 1. Build enriched context (keyword + entity graph + semantic recall)
        let built_context = self
            .context_builder
            .build_with_semantic(context, query, author_id)
            .await?;

        // 2. Store the user's query + write-path fact harvest: regex fast-path
        //    synchronously (deterministic, free), LLM extraction+consolidation
        //    in the background so the reply is never delayed.
        self.memory_manager.store_user_message(context, query, author_id)?;
        if let Some(user_id) = author_id {
            let _ = self.extract_user_facts(user_id, query, context);
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let extractor = self.fact_extractor.clone();
                let (q, uid, ctx) = (query.to_string(), *user_id, context.clone());
                handle.spawn(async move {
                    match extractor.extract_and_store(&q, &uid, &ctx).await {
                        Ok(n) if n > 0 => tracing::debug!(facts = n, "LLM write path stored facts"),
                        Ok(_) => {}
                        Err(e) => tracing::debug!("LLM fact extraction skipped: {e}"),
                    }
                });
            }
        }

        // 3. Build the real chat-turn array: system (persona + memory), then
        //    the stored conversation as genuine user/assistant turns, then the
        //    new query. Chat models are trained on this shape — history-as-a-
        //    text-blob loses multi-turn coherence.
        use crate::local_ai::backend::{ChatMessage, ChatRole};

        let persona = "You are Jarvis, the user's personal AI assistant running \
            locally on their device. You have persistent memory: facts the user \
            shares are stored automatically, so when asked to remember something, \
            simply confirm it. Answer directly and use the context below when \
            relevant.";
        let system_context = built_context.to_system_prompt();
        let mut system = if system_context.is_empty() {
            persona.to_string()
        } else {
            format!("{persona}\n\n{system_context}")
        };
        if let Some(app_context) = &self.app_context {
            let app = app_context();
            if !app.is_empty() {
                system.push_str(
                    "\n\n## App Context\nLive data from the user's Chatter app. \
                     Answer questions about their channels and friends directly \
                     from this data:\n",
                );
                system.push_str(&app);
            }
        }

        let mut messages = Vec::with_capacity(built_context.history.len() + 2);
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: system,
        });
        for turn in &built_context.history {
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

        // 4. Generate response (budget covers thinking models' <think> spend)
        let params = SampleParams {
            max_tokens: 1024,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.1,
            disable_thinking: false,
            json_schema: None,
        };

        // Policy gate: a DM or anon conversation refuses a cloud engine here,
        // before any message content leaves the device.
        let access = crate::ai_policy::policy::ai_access(&context.conversation_type(), false);
        let response = self
            .ai
            .complete_conversation_in(access, silo, messages, params)
            .await?;
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
            disable_thinking: true,
            json_schema: None,
        };

        let access = crate::ai_policy::policy::ai_access(&context.conversation_type(), false);
        self.ai
            .complete_gated(access, prompt, params)
            .await
            .map_err(JarvisError::from)
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
