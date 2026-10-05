//! Memory management for Jarvis.
//!
//! Handles memory lifecycle:
//! - Storing conversations and facts
//! - Applying decay to old facts
//! - Compressing old conversations into summaries
//! - Clearing a context's history and reporting memory statistics

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::jarvis::{
    JarvisContext, JarvisConversation, JarvisFact, JarvisSummary, JarvisSummaryId, Role,
};
use crate::services::ai::SampleParams;
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use super::context_builder::HISTORY_TURN_LIMIT;
use super::engine::SharedJarvisEngine;
use super::error::JarvisError;
use crate::ai_policy::policy::AiAccess;

/// Memory manager for Jarvis.
#[derive(Clone)]
pub struct MemoryManager {
    memory_repo: Arc<JarvisMemoryRepository>,
    graph_repo: Arc<JarvisGraphRepository>,
    ai: SharedJarvisEngine,
}

/// Configuration for memory compression.
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    /// Minimum messages a summary must cover.
    pub min_messages: u32,
    /// Stored turns at which compression runs.
    pub max_messages: u32,
    /// Messages to keep uncompressed.
    pub keep_recent: u32,
}

impl Default for CompressionConfig {
    /// Compress once the stored history reaches the turn window the context
    /// builder reads (`HISTORY_TURN_LIMIT`). Older turns would otherwise drop
    /// out of the model's context silently; summarised, they stay in it via
    /// "Previous Conversation Summaries".
    fn default() -> Self {
        Self {
            min_messages: 20,
            max_messages: HISTORY_TURN_LIMIT,
            keep_recent: 20,
        }
    }
}

/// Statistics about Jarvis memory for a context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStats {
    /// Number of stored conversations.
    pub conversation_count: u32,
    /// Number of summaries.
    pub summary_count: u32,
    /// Total messages that have been summarized.
    pub total_messages_summarized: u32,
    /// Number of current (not superseded) learned facts, across all contexts.
    pub fact_count: u32,
    /// Number of entities in knowledge graph.
    pub entity_count: u32,
}

impl MemoryManager {
    /// Create a new memory manager.
    pub fn new(
        memory_repo: Arc<JarvisMemoryRepository>,
        graph_repo: Arc<JarvisGraphRepository>,
        ai: SharedJarvisEngine,
    ) -> Self {
        Self {
            memory_repo,
            graph_repo,
            ai,
        }
    }

    /// Store a user message in memory.
    pub fn store_user_message(
        &self,
        context: &JarvisContext,
        content: &str,
        author_id: Option<&UserId>,
    ) -> Result<(), JarvisError> {
        let conv = if let Some(user_id) = author_id {
            JarvisConversation::user_message(context, content.to_string()).with_author(user_id)
        } else {
            JarvisConversation::user_message(context, content.to_string())
        };

        self.memory_repo.insert_conversation(&conv)?;
        Ok(())
    }

    /// Store an assistant response in memory.
    pub fn store_assistant_message(
        &self,
        context: &JarvisContext,
        content: &str,
    ) -> Result<(), JarvisError> {
        let conv = JarvisConversation::assistant_message(context, content.to_string());
        self.memory_repo.insert_conversation(&conv)?;
        Ok(())
    }

    /// Store a fact learned from a message.
    pub fn store_fact(&self, fact: &JarvisFact) -> Result<(), JarvisError> {
        self.memory_repo.upsert_fact(fact)?;
        Ok(())
    }

    /// Apply decay to all facts and prune low-confidence ones.
    ///
    /// Should be called periodically (e.g., daily).
    pub fn apply_decay(&self, threshold: f64) -> Result<u32, JarvisError> {
        let pruned = self.memory_repo.apply_decay(threshold)?;
        tracing::info!(pruned = pruned, "Applied fact decay");
        Ok(pruned)
    }

    /// Clear all conversation history (turns and their summaries) for a
    /// context. Facts and the knowledge graph are global and stay. Returns
    /// the number of conversation turns deleted.
    pub fn clear_context(&self, context: &JarvisContext) -> Result<u32, JarvisError> {
        let deleted = self.memory_repo.delete_conversations_for_context(context)?;
        self.memory_repo.delete_summaries_for_context(context)?;
        tracing::info!(context_id = %context.context_id, deleted = deleted, "Cleared Jarvis context history");
        Ok(deleted)
    }

    /// Compress old conversations into summaries.
    ///
    /// Once `max_messages` turns are stored, summarizes the oldest ones
    /// (all but `keep_recent`) with the LLM, stores the summary and deletes
    /// the summarized turns. Nothing is deleted unless a non-empty summary
    /// was produced.
    pub async fn compress_if_needed(
        &self,
        context: &JarvisContext,
        config: &CompressionConfig,
    ) -> Result<Option<JarvisSummary>, JarvisError> {
        let Some(batch) = self.select_for_compression(context, config)? else {
            return Ok(None);
        };
        let access = crate::ai_policy::policy::ai_access(&context.conversation_type(), false);
        let summary_text = self.generate_summary(access, &batch).await?;
        if summary_text.is_empty() {
            tracing::warn!(context_id = %context.context_id, "Empty summary; history left uncompressed");
            return Ok(None);
        }
        self.apply_compression(context, &batch, summary_text)
            .map(Some)
    }

    /// The oldest turns to summarize, or `None` when the context is under
    /// its budget or the batch would be smaller than `min_messages`.
    pub(crate) fn select_for_compression(
        &self,
        context: &JarvisContext,
        config: &CompressionConfig,
    ) -> Result<Option<Vec<JarvisConversation>>, JarvisError> {
        let count = self.memory_repo.count_conversations(context)?;
        if count < config.max_messages {
            return Ok(None);
        }
        let to_compress = count.saturating_sub(config.keep_recent);
        if to_compress < config.min_messages {
            return Ok(None);
        }
        let batch = self
            .memory_repo
            .get_oldest_conversations(context, to_compress)?;
        Ok((!batch.is_empty()).then_some(batch))
    }

    /// Store `summary_text` for `batch` and delete the summarized turns.
    pub(crate) fn apply_compression(
        &self,
        context: &JarvisContext,
        batch: &[JarvisConversation],
        summary_text: String,
    ) -> Result<JarvisSummary, JarvisError> {
        let message_range_start = batch.first().map(|c| c.timestamp).unwrap_or(0);
        let message_range_end = batch.last().map(|c| c.timestamp).unwrap_or(0);

        let summary = JarvisSummary {
            id: JarvisSummaryId::new(),
            context_type: context.context_type,
            context_id: context.context_id.clone(),
            summary: summary_text,
            message_range_start,
            message_range_end,
            message_count: batch.len() as u32,
            created_at: chrono::Utc::now(),
        };

        self.memory_repo.insert_summary(&summary)?;
        self.memory_repo.delete_conversations_in_range(
            context,
            message_range_start,
            message_range_end,
        )?;

        tracing::info!(
            context_id = %context.context_id,
            message_count = batch.len(),
            "Compressed conversations into summary"
        );

        Ok(summary)
    }

    /// Generate a summary of conversations using the LLM.
    async fn generate_summary(
        &self,
        access: AiAccess,
        conversations: &[JarvisConversation],
    ) -> Result<String, JarvisError> {
        let conversation_text: Vec<String> = conversations
            .iter()
            .map(|c| {
                let role = match c.role {
                    Role::User => "User",
                    Role::Assistant => "Jarvis",
                    Role::System => "System",
                };
                format!("{}: {}", role, c.content)
            })
            .collect();

        let prompt = format!(
            r#"Summarize this conversation in 2-3 sentences, focusing on:
- Key topics discussed
- Important decisions or conclusions
- Any facts learned about the user

Conversation:
{}

Summary:"#,
            conversation_text.join("\n")
        );

        let params = SampleParams {
            max_tokens: 150,
            temperature: 0.3,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.0,
            disable_thinking: true,
            json_schema: None,
        };

        let summary = self.ai.complete_gated(access, prompt, params).await?;
        Ok(summary.trim().to_string())
    }

    /// Get memory statistics for a context.
    pub fn get_stats(&self, context: &JarvisContext) -> Result<MemoryStats, JarvisError> {
        let conversation_count = self.memory_repo.count_conversations(context)?;
        let summaries = self.memory_repo.get_summaries(context, 100)?;
        let fact_count = self.memory_repo.search_facts("", 10000)?.len() as u32;
        let entity_count = self.graph_repo.count_entities()?;

        let total_messages_summarized = summaries.iter().map(|s| s.message_count).sum();

        Ok(MemoryStats {
            conversation_count,
            summary_count: summaries.len() as u32,
            total_messages_summarized,
            fact_count,
            entity_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_ai::settings::LocalAISettings;
    use crate::services::ai::LocalAIActorHandle;
    use crate::services::jarvis::engine::JarvisEngine;
    use crate::storage::Database;

    fn manager() -> (
        MemoryManager,
        Arc<JarvisMemoryRepository>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Arc::new(Database::open(&dir.path().join("jarvis.db"), "test-passphrase").unwrap());
        db.run_migrations().unwrap();
        let memory = Arc::new(JarvisMemoryRepository::new(db.clone()));
        let engine = SharedJarvisEngine::new(JarvisEngine::from_settings(
            &LocalAISettings::default(),
            LocalAIActorHandle::spawn(),
        ));
        let mm = MemoryManager::new(
            memory.clone(),
            Arc::new(JarvisGraphRepository::new(db)),
            engine,
        );
        (mm, memory, dir)
    }

    /// Insert `n` alternating turns with strictly increasing timestamps.
    fn seed(memory: &JarvisMemoryRepository, ctx: &JarvisContext, n: u32) {
        for i in 0..n {
            let mut c = if i % 2 == 0 {
                JarvisConversation::user_message(ctx, format!("turn {i}"))
            } else {
                JarvisConversation::assistant_message(ctx, format!("turn {i}"))
            };
            c.timestamp = 1_000 + u64::from(i);
            memory.insert_conversation(&c).unwrap();
        }
    }

    #[test]
    fn compression_triggers_at_the_context_builder_window() {
        let config = CompressionConfig::default();
        assert_eq!(config.max_messages, HISTORY_TURN_LIMIT);
        assert!(config.keep_recent + config.min_messages <= config.max_messages);
    }

    #[test]
    fn under_budget_nothing_is_selected() {
        let (mm, memory, _d) = manager();
        let ctx = JarvisContext::general();
        seed(&memory, &ctx, HISTORY_TURN_LIMIT - 1);
        assert!(
            mm.select_for_compression(&ctx, &CompressionConfig::default())
                .unwrap()
                .is_none()
        );
    }

    /// Over budget, the OLDEST turns are summarized and deleted; the most
    /// recent `keep_recent` stay as real turns, and the summary is what the
    /// context builder will now read.
    #[test]
    fn over_budget_the_oldest_turns_become_a_summary() {
        let (mm, memory, _d) = manager();
        let ctx = JarvisContext::general();
        let other = JarvisContext::channel(crate::domain::ids::ChannelId::new());
        seed(&memory, &ctx, HISTORY_TURN_LIMIT);
        seed(&memory, &other, 5);
        let config = CompressionConfig::default();

        let batch = mm
            .select_for_compression(&ctx, &config)
            .unwrap()
            .expect("over budget");
        assert_eq!(batch.len() as u32, HISTORY_TURN_LIMIT - config.keep_recent);
        assert_eq!(batch.first().unwrap().content, "turn 0");

        let summary = mm
            .apply_compression(&ctx, &batch, "they talked".into())
            .unwrap();
        assert_eq!(summary.message_count, batch.len() as u32);

        let left = memory.get_recent_conversations(&ctx, 1000).unwrap();
        assert_eq!(left.len() as u32, config.keep_recent);
        assert_eq!(
            left.first().unwrap().content,
            format!("turn {}", HISTORY_TURN_LIMIT - config.keep_recent)
        );
        assert_eq!(
            memory.get_summaries(&ctx, 10).unwrap()[0].summary,
            "they talked"
        );
        // Another context is untouched.
        assert_eq!(memory.count_conversations(&other).unwrap(), 5);

        let stats = mm.get_stats(&ctx).unwrap();
        assert_eq!(stats.conversation_count, config.keep_recent);
        assert_eq!(stats.summary_count, 1);
        assert_eq!(stats.total_messages_summarized, batch.len() as u32);
        // Now under budget again.
        assert!(mm.select_for_compression(&ctx, &config).unwrap().is_none());
    }

    #[test]
    fn clear_context_deletes_turns_and_summaries_of_that_context_only() {
        let (mm, memory, _d) = manager();
        let ctx = JarvisContext::general();
        let other = JarvisContext::channel(crate::domain::ids::ChannelId::new());
        seed(&memory, &ctx, HISTORY_TURN_LIMIT);
        seed(&memory, &other, 3);
        let batch = mm
            .select_for_compression(&ctx, &CompressionConfig::default())
            .unwrap()
            .unwrap();
        mm.apply_compression(&ctx, &batch, "s".into()).unwrap();

        let deleted = mm.clear_context(&ctx).unwrap();
        assert_eq!(deleted, CompressionConfig::default().keep_recent);
        let stats = mm.get_stats(&ctx).unwrap();
        assert_eq!((stats.conversation_count, stats.summary_count), (0, 0));
        assert_eq!(memory.count_conversations(&other).unwrap(), 3);
    }
}
