//! Memory management for Jarvis.
//!
//! Handles memory lifecycle:
//! - Storing conversations
//! - Clearing a context's history and reporting memory statistics

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::jarvis::{JarvisContext, JarvisConversation};
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};

use super::error::JarvisError;

/// Memory manager for Jarvis.
#[derive(Clone)]
pub struct MemoryManager {
    memory_repo: Arc<JarvisMemoryRepository>,
    graph_repo: Arc<JarvisGraphRepository>,
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
    ) -> Self {
        Self {
            memory_repo,
            graph_repo,
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

    /// Clear all conversation history (turns and their summaries) for a
    /// context. Facts and the knowledge graph are global and stay. Returns
    /// the number of conversation turns deleted.
    pub fn clear_context(&self, context: &JarvisContext) -> Result<u32, JarvisError> {
        let deleted = self.memory_repo.delete_conversations_for_context(context)?;
        self.memory_repo.delete_summaries_for_context(context)?;
        tracing::info!(context_id = %context.context_id, deleted = deleted, "Cleared Jarvis context history");
        Ok(deleted)
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
        let mm = MemoryManager::new(memory.clone(), Arc::new(JarvisGraphRepository::new(db)));
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
    fn clear_context_deletes_turns_of_that_context_only() {
        let (mm, memory, _d) = manager();
        let ctx = JarvisContext::general();
        let other = JarvisContext::channel(crate::domain::ids::ChannelId::new());
        seed(&memory, &ctx, 7);
        seed(&memory, &other, 3);

        let deleted = mm.clear_context(&ctx).unwrap();
        assert_eq!(deleted, 7);
        let stats = mm.get_stats(&ctx).unwrap();
        assert_eq!((stats.conversation_count, stats.summary_count), (0, 0));
        assert_eq!(memory.count_conversations(&other).unwrap(), 3);
    }
}
