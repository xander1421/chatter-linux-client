//! Context builder for the remote assistant: the owner's own remote
//! conversation history, nothing else.

use std::sync::Arc;

use crate::domain::UserId;
use crate::domain::jarvis::{JarvisContext, JarvisConversation};

use crate::storage::repositories::JarvisMemoryRepository;

use super::error::JarvisError;

/// Most recent stored turns the builder reads as chat history.
pub(crate) const HISTORY_TURN_LIMIT: u32 = 60;

/// Context builder for enriching Jarvis queries.
pub struct ContextBuilder {
    memory_repo: Arc<JarvisMemoryRepository>,
}

/// Built context for a Jarvis query.
#[derive(Debug, Default)]
pub struct BuiltContext {
    /// Recent conversation history.
    pub history: Vec<JarvisConversation>,
}

impl ContextBuilder {
    /// Create a new context builder.
    pub fn new(memory_repo: Arc<JarvisMemoryRepository>) -> Self {
        Self { memory_repo }
    }

    /// Remote context includes only this owner's new remote conversation.
    /// Legacy facts, entities, summaries and local history never enter the prompt.
    pub fn build_remote(
        &self,
        context: &JarvisContext,
        owner: &UserId,
    ) -> Result<BuiltContext, JarvisError> {
        let remote = context.for_remote(owner);
        let mut history = self
            .memory_repo
            .get_recent_conversations(&remote, HISTORY_TURN_LIMIT)?;
        let mut bytes: usize = history.iter().map(|turn| turn.content.len()).sum();
        while bytes > 32_000 && history.len() > 2 {
            bytes -= history.remove(0).content.len();
        }
        Ok(BuiltContext {
            history,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_history_excludes_legacy_memory_other_owners_and_other_contexts() {
        use crate::storage::Database;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("privacy.db"), "fixture-pass").unwrap());
        db.run_migrations().unwrap();
        let memory = Arc::new(JarvisMemoryRepository::new(db.clone()));
        let builder = ContextBuilder::new(memory.clone());
        let owner = UserId::new();
        let other = UserId::new();
        let context = JarvisContext::general();
        let remote = context.for_remote(&owner);
        assert_eq!(remote.for_remote(&owner), remote);
        for (ctx, text) in [
            (context.clone(), "legacy private conversation"),
            (context.for_remote(&other), "other owner conversation"),
            (
                JarvisContext::channel(crate::domain::ChannelId::new()).for_remote(&owner),
                "other channel conversation",
            ),
            (remote.clone(), "permitted remote history"),
        ] {
            memory
                .insert_conversation(&JarvisConversation::user_message(&ctx, text.into()))
                .unwrap();
        }
        let built = builder.build_remote(&context, &owner).unwrap();
        assert_eq!(built.history.len(), 1);
        assert_eq!(built.history[0].content, "permitted remote history");
        assert_eq!(
            memory.get_recent_conversations(&context, 10).unwrap().len(),
            1
        );
    }
}
