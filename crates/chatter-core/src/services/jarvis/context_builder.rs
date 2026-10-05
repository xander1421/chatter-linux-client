//! Context builder for RAG-style retrieval.
//!
//! Builds enriched context for Jarvis responses by combining:
//! - Recent conversation history
//! - Relevant facts about the user
//! - Related entities from the knowledge graph
//! - Conversation summaries

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::jarvis::{
    EntityKnowledge, JarvisContext, JarvisConversation, JarvisFact, JarvisSummary, SubjectType,
};
use crate::local_ai::vector_store::VectorSearchResult;
use crate::services::semantic_search::SemanticSearchService;
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use super::error::JarvisError;

/// Most recent stored turns the builder reads as chat history. Turns older
/// than this window reach the model only through a summary
/// (`MemoryManager::compress_if_needed`).
pub(crate) const HISTORY_TURN_LIMIT: u32 = 60;

/// Context builder for enriching Jarvis queries.
pub struct ContextBuilder {
    memory_repo: Arc<JarvisMemoryRepository>,
    graph_repo: Arc<JarvisGraphRepository>,
    /// Embedding-based recall over indexed channel messages. `None` when the
    /// active provider has no embedding support (e.g. mistralrs-only setup) —
    /// retrieval then degrades to keyword facts + entity graph.
    semantic: Option<Arc<SemanticSearchService>>,
}

/// Built context for a Jarvis query.
#[derive(Debug, Default)]
pub struct BuiltContext {
    /// Recent conversation history.
    pub history: Vec<JarvisConversation>,
    /// Facts about the user.
    pub user_facts: Vec<JarvisFact>,
    /// Facts relevant to the query.
    pub query_facts: Vec<JarvisFact>,
    /// Related entity knowledge.
    pub entity_knowledge: Vec<EntityKnowledge>,
    /// Conversation summaries.
    pub summaries: Vec<JarvisSummary>,
    /// Semantically similar past messages (embedding recall).
    pub semantic_messages: Vec<VectorSearchResult>,
}

impl BuiltContext {
    /// Format the retrieved memory (facts, entities, semantic hits, summaries)
    /// as a system-prompt section. Conversation history is deliberately NOT
    /// rendered here — it goes to the model as real chat turns instead
    /// (`JarvisService::ask` builds the message array).
    pub fn to_system_prompt(&self) -> String {
        let mut parts = Vec::new();

        // Add user facts (temporally current — superseded facts are filtered
        // at the repository; render since-when for context)
        if !self.user_facts.is_empty() {
            parts.push("\n## About the User".to_string());
            for fact in &self.user_facts {
                if fact.decayed_confidence() > 0.3 {
                    match fact.valid_from {
                        Some(from) => parts.push(format!(
                            "- {}: {} (since {})",
                            fact.predicate,
                            fact.object,
                            from.format("%Y-%m-%d")
                        )),
                        None => parts.push(format!("- {}: {}", fact.predicate, fact.object)),
                    }
                }
            }
        }

        // Add query-relevant facts
        if !self.query_facts.is_empty() {
            parts.push("\n## Relevant Knowledge".to_string());
            for fact in &self.query_facts {
                if fact.decayed_confidence() > 0.3 {
                    parts.push(format!(
                        "- {} {} {}",
                        fact.subject_id, fact.predicate, fact.object
                    ));
                }
            }
        }

        // Add entity knowledge
        if !self.entity_knowledge.is_empty() {
            parts.push("\n## Related Entities".to_string());
            for knowledge in &self.entity_knowledge {
                let ctx = knowledge.to_context_string();
                if !ctx.is_empty() {
                    parts.push(ctx);
                }
            }
        }

        // Add semantically similar past messages
        if !self.semantic_messages.is_empty() {
            parts.push("\n## Related Past Messages".to_string());
            for hit in &self.semantic_messages {
                parts.push(format!("- {}", hit.content_preview));
            }
        }

        // Add summaries
        if !self.summaries.is_empty() {
            parts.push("\n## Previous Conversation Summaries".to_string());
            for summary in &self.summaries {
                parts.push(format!("- {}", summary.summary));
            }
        }

        if parts.is_empty() {
            String::new()
        } else {
            parts.join("\n")
        }
    }

    /// Check if the context is empty.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
            && self.user_facts.is_empty()
            && self.query_facts.is_empty()
            && self.entity_knowledge.is_empty()
            && self.summaries.is_empty()
            && self.semantic_messages.is_empty()
    }

    /// Get the total token estimate for this context.
    ///
    /// Very rough estimate: ~4 chars per token.
    pub fn estimated_tokens(&self) -> usize {
        let prompt = self.to_system_prompt();
        prompt.len() / 4
    }
}

impl ContextBuilder {
    /// Create a new context builder.
    pub fn new(
        memory_repo: Arc<JarvisMemoryRepository>,
        graph_repo: Arc<JarvisGraphRepository>,
        semantic: Option<Arc<SemanticSearchService>>,
    ) -> Self {
        Self {
            memory_repo,
            graph_repo,
            semantic,
        }
    }

    /// Build context for a query.
    ///
    /// Retrieves relevant information from memory and the knowledge graph.
    #[allow(clippy::field_reassign_with_default)]
    pub fn build(
        &self,
        context: &JarvisContext,
        query: &str,
        author_id: Option<&UserId>,
    ) -> Result<BuiltContext, JarvisError> {
        let mut built = BuiltContext::default();

        // 1. Recent conversation history, budgeted by size rather than a fixed
        //    turn count: fetch generously, then trim oldest-first to ~8K tokens
        //    (~4 chars/token) so long chats never blow the model context.
        const HISTORY_CHAR_BUDGET: usize = 32_000;
        let mut history = self
            .memory_repo
            .get_recent_conversations(context, HISTORY_TURN_LIMIT)?;
        let mut total: usize = history.iter().map(|c| c.content.len()).sum();
        while total > HISTORY_CHAR_BUDGET && history.len() > 2 {
            total -= history.remove(0).content.len();
        }
        built.history = history;

        // 2. Get facts about the user
        if let Some(user_id) = author_id {
            built.user_facts = self
                .memory_repo
                .get_facts_about(SubjectType::User, &user_id.to_string())?;
        }

        // 3. Search for query-relevant facts
        built.query_facts = self.memory_repo.search_facts(query, 10)?;

        // 4. Get related entities from the query
        let entities = self.extract_entity_names(query);
        for entity_name in entities {
            let knowledge = self
                .graph_repo
                .get_entity_knowledge(&entity_name, &self.memory_repo)?;
            if knowledge.entity.is_some() {
                built.entity_knowledge.push(knowledge);
            }
        }

        // 5. Get conversation summaries
        built.summaries = self.memory_repo.get_summaries(context, 5)?;

        Ok(built)
    }

    /// `build` plus embedding-based recall of similar past messages.
    ///
    /// A semantic-search failure (provider down, no embedding model) is
    /// logged and skipped — memory retrieval must never fail an ask.
    pub async fn build_with_semantic(
        &self,
        context: &JarvisContext,
        query: &str,
        author_id: Option<&UserId>,
    ) -> Result<BuiltContext, JarvisError> {
        let mut built = self.build(context, query, author_id)?;
        if let Some(semantic) = &self.semantic {
            match semantic.search_hybrid(query, 5).await {
                Ok(hits) => built.semantic_messages = hits,
                Err(e) => tracing::debug!("hybrid recall skipped: {e}"),
            }
        }
        Ok(built)
    }

    /// Build minimal context (just recent history).
    ///
    /// Use when you want fast context without full RAG retrieval.
    pub fn build_minimal(
        &self,
        context: &JarvisContext,
        limit: u32,
    ) -> Result<BuiltContext, JarvisError> {
        let history = self.memory_repo.get_recent_conversations(context, limit)?;
        Ok(BuiltContext {
            history,
            ..Default::default()
        })
    }

    /// Extract potential entity names from a query.
    ///
    /// Simple heuristic: look for capitalized words that might be names.
    fn extract_entity_names(&self, query: &str) -> Vec<String> {
        let mut names = Vec::new();

        for word in query.split_whitespace() {
            // Skip short words and common words
            if word.len() < 3 {
                continue;
            }

            let clean = word.trim_matches(|c: char| !c.is_alphanumeric());
            if clean.is_empty() {
                continue;
            }

            // Check if it looks like a proper noun (capitalized)
            if clean.chars().next().is_some_and(|c| c.is_uppercase()) {
                names.push(clean.to_lowercase());
            }
        }

        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::jarvis::Role;

    #[test]
    fn test_history_is_not_rendered_into_system_prompt() {
        // History goes to the model as real chat turns (JarvisService::ask),
        // never as system-prompt text.
        let context = BuiltContext {
            history: vec![JarvisConversation {
                id: crate::domain::JarvisConversationId::new(),
                context_type: crate::domain::ContextType::Channel,
                context_id: "test".to_string(),
                role: Role::User,
                content: "Hello".to_string(),
                timestamp: 0,
                metadata_json: None,
                created_at: chrono::Utc::now(),
            }],
            ..Default::default()
        };

        assert!(!context.is_empty(), "history still counts as context");
        let prompt = context.to_system_prompt();
        assert!(
            !prompt.contains("Hello"),
            "history leaked into the system prompt"
        );
    }

    #[test]
    fn test_semantic_hits_render_in_prompt() {
        use crate::local_ai::vector_store::VectorSearchResult;
        use crate::storage::repositories::vector_repo::EmbeddingSourceType;
        let context = BuiltContext {
            semantic_messages: vec![VectorSearchResult {
                source_id: "m1".into(),
                source_type: EmbeddingSourceType::Message,
                channel_id: Some("c1".into()),
                content_preview: "we agreed to ship Friday".into(),
                score: 0.82,
            }],
            ..Default::default()
        };
        assert!(!context.is_empty());
        let prompt = context.to_system_prompt();
        assert!(prompt.contains("Related Past Messages"));
        assert!(prompt.contains("we agreed to ship Friday"));
    }

    #[test]
    fn test_empty_context() {
        let context = BuiltContext::default();
        assert!(context.is_empty());
        assert!(context.to_system_prompt().is_empty());
    }

    #[test]
    fn test_estimated_tokens() {
        use crate::local_ai::vector_store::VectorSearchResult;
        use crate::storage::repositories::vector_repo::EmbeddingSourceType;
        let mut context = BuiltContext::default();
        context.semantic_messages.push(VectorSearchResult {
            source_id: "m1".into(),
            source_type: EmbeddingSourceType::Message,
            channel_id: None,
            content_preview: "A".repeat(400), // ~100 tokens
            score: 0.9,
        });

        let tokens = context.estimated_tokens();
        assert!(tokens > 0);
    }
}
