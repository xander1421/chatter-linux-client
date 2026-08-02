//! Conversation pin service.
//!
//! Pinning is a **local-only** preference — it never reaches the server and is
//! not synced between devices. The state lives in the settings repo under
//! `conv.pinned.<conversation_id>`, keyed by the same id the sidebar uses for
//! that conversation (channel id for channels, peer id for server DMs, the anon
//! id for anon DMs/groups).
//!
//! Ordering lives here rather than in the UI: every sidebar list asks this
//! service which ids are pinned and sorts with [`ConversationPins::sort_pinned_first`].

use std::collections::HashSet;
use std::sync::Arc;

use crate::error::StorageError;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Settings-key prefix for the per-conversation pin flag.
const PIN_KEY_PREFIX: &str = "conv.pinned.";

/// Local-only conversation pinning.
pub struct ConversationPins {
    settings: Arc<SettingsRepository>,
}

impl ConversationPins {
    /// Create a new pin service.
    pub fn new(settings: Arc<SettingsRepository>) -> Self {
        Self { settings }
    }

    /// The settings key holding a conversation's pin flag.
    fn key(conversation_id: &str) -> String {
        format!("{PIN_KEY_PREFIX}{conversation_id}")
    }

    /// Pin or unpin a conversation. Unpinning deletes the key rather than
    /// writing `false`, so `pinned_ids` stays a clean set.
    pub fn set_pinned(&self, conversation_id: &str, pinned: bool) -> Result<(), StorageError> {
        let key = Self::key(conversation_id);
        if pinned {
            self.settings.set_bool(&key, true)
        } else {
            self.settings.delete(&key).map(|_| ())
        }
    }

    /// Whether a conversation is pinned.
    pub fn is_pinned(&self, conversation_id: &str) -> bool {
        self.settings
            .get_bool(&Self::key(conversation_id))
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Every pinned conversation id, in one query. Use this when flagging a
    /// whole list — `is_pinned` per row is a select per row.
    pub fn pinned_ids(&self) -> HashSet<String> {
        let Ok(all) = self.settings.get_all() else {
            return HashSet::new();
        };
        all.into_iter()
            .filter_map(|(key, value)| {
                let id = key.strip_prefix(PIN_KEY_PREFIX)?;
                // Values are JSON; `true` is the only pinned state we write.
                (value.trim() == "true").then(|| id.to_string())
            })
            .collect()
    }

    /// Stable-sort a list so pinned entries come first, preserving the caller's
    /// existing order (MRU, alphabetical, …) within each group.
    pub fn sort_pinned_first<T, F>(&self, items: &mut [T], id_of: F)
    where
        F: Fn(&T) -> String,
    {
        let pinned = self.pinned_ids();
        if pinned.is_empty() {
            return;
        }
        items.sort_by_key(|item| !pinned.contains(&id_of(item)));
    }
}
