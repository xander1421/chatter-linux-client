//! Memory management for Jarvis.
//!
//! Handles memory lifecycle:
//! - Storing conversations and facts
//! - Applying decay to old facts
//! - Compressing old conversations into summaries

use std::sync::Arc;

use crate::domain::jarvis::{
    JarvisContext, JarvisConversation, JarvisFact, JarvisSummary, Role,
};
use crate::domain::UserId;
use crate::services::ai::{LocalAIActorHandle, SampleParams};
use crate::storage::repositories::JarvisMemoryRepository;

use super::error::JarvisError;

/// Memory manager for Jarvis.
pub struct MemoryManager {
    memory_repo: Arc<JarvisMemoryRepository>,
    ai: LocalAIActorHandle,
}

/// Configuration for memory compression.
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    /// Minimum messages before considering compression.
    pub min_messages: u32,
    /// Maximum messages before forcing compression.
    pub max_messages: u32,
    /// Messages to keep uncompressed.
    pub keep_recent: u32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            min_messages: 50,
            max_messages: 200,
            keep_recent: 20,
        }
    }
}

impl MemoryManager {
    /// Create a new memory manager.
    pub fn new(memory_repo: Arc<JarvisMemoryRepository>, ai: LocalAIActorHandle) -> Self {
        Self { memory_repo, ai }
    }

    /// Store a user message in memory.
    pub fn store_user_message(
        &self,
        context: &JarvisContext,
        content: &str,
        author_id: Option<&UserId>,
    ) -> Result<(), JarvisError> {
        let conv = if let Some(user_id) = author_id {
            JarvisConversation::user_message(context, content.to_string())
                .with_author(user_id)
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

    /// Compress old conversations into summaries.
    ///
    /// If there are more than `max_messages` conversations, summarizes
    /// the oldest ones and deletes them, keeping `keep_recent` uncompressed.
    pub async fn compress_if_needed(
        &self,
        context: &JarvisContext,
        config: &CompressionConfig,
    ) -> Result<Option<JarvisSummary>, JarvisError> {
        let count = self.memory_repo.count_conversations(context)?;

        if count < config.max_messages {
            return Ok(None);
        }

        // Get conversations to compress (oldest ones, excluding keep_recent)
        let to_compress_count = count.saturating_sub(config.keep_recent);
        if to_compress_count < config.min_messages {
            return Ok(None);
        }

        // Get the oldest conversations
        let latest_summary_end = self.memory_repo.get_latest_summary_end(context)?;
        let conversations = if let Some(after) = latest_summary_end {
            self.memory_repo
                .get_conversations_before(context, u64::MAX, to_compress_count)?
                .into_iter()
                .filter(|c| c.timestamp > after)
                .collect()
        } else {
            self.memory_repo
                .get_conversations_before(context, u64::MAX, to_compress_count)?
        };

        if conversations.is_empty() {
            return Ok(None);
        }

        // Generate summary using LLM
        let summary_text = self.generate_summary(&conversations).await?;

        let message_range_start = conversations.first().map(|c| c.timestamp).unwrap_or(0);
        let message_range_end = conversations.last().map(|c| c.timestamp).unwrap_or(0);

        let summary = JarvisSummary::new(
            context,
            summary_text,
            message_range_start,
            message_range_end,
            conversations.len() as u32,
        );

        // Store summary
        self.memory_repo.insert_summary(&summary)?;

        // Delete compressed conversations
        self.memory_repo.delete_conversations_in_range(
            context,
            message_range_start,
            message_range_end,
        )?;

        tracing::info!(
            context_id = %context.context_id,
            message_count = conversations.len(),
            "Compressed conversations into summary"
        );

        Ok(Some(summary))
    }

    /// Generate a summary of conversations using the LLM.
    async fn generate_summary(
        &self,
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
        };

        let summary = self.ai.complete(prompt, params).await?;
        Ok(summary.trim().to_string())
    }

    /// Get memory statistics for a context.
    pub fn get_stats(&self, context: &JarvisContext) -> Result<MemoryStats, JarvisError> {
        let conversation_count = self.memory_repo.count_conversations(context)?;
        let summaries = self.memory_repo.get_summaries(context, 100)?;

        let total_summarized = summaries.iter().map(|s| s.message_count).sum();

        Ok(MemoryStats {
            conversation_count,
            summary_count: summaries.len() as u32,
            total_messages_summarized: total_summarized,
        })
    }
}

/// Statistics about Jarvis memory for a context.
#[derive(Debug, Clone)]
pub struct MemoryStats {
    /// Number of stored conversations.
    pub conversation_count: u32,
    /// Number of summaries.
    pub summary_count: u32,
    /// Total messages that have been summarized.
    pub total_messages_summarized: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_config_default() {
        let config = CompressionConfig::default();
        assert_eq!(config.min_messages, 50);
        assert_eq!(config.max_messages, 200);
        assert_eq!(config.keep_recent, 20);
    }
}
