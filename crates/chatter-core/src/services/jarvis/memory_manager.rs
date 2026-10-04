//! Memory management for Jarvis.
//!
//! Handles memory lifecycle:
//! - Storing conversations and facts
//! - Applying decay to old facts

use std::sync::Arc;

use crate::domain::jarvis::{JarvisContext, JarvisConversation, JarvisFact};
use crate::domain::UserId;
use crate::storage::repositories::JarvisMemoryRepository;

use super::error::JarvisError;

/// Memory manager for Jarvis.
pub struct MemoryManager {
    memory_repo: Arc<JarvisMemoryRepository>,
}

impl MemoryManager {
    /// Create a new memory manager.
    pub fn new(memory_repo: Arc<JarvisMemoryRepository>) -> Self {
        Self { memory_repo }
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
}
