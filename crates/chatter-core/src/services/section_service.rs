//! Section service.
//!
//! Manage custom sidebar sections and their conversation memberships.

use std::sync::Arc;

use crate::domain::ids::SectionId;
use crate::domain::message::ConversationType;
use crate::domain::section::{CustomSection, SectionConversation, SectionSortMode};
use crate::error::StorageError;
use crate::services::conversation_pins::ConversationPins;
use crate::storage::repositories::section_repo::SectionRepository;
use crate::storage::repositories::unified_conversation_repo::UnifiedConversationRepository;

/// Section service errors.
#[derive(Debug, thiserror::Error)]
pub enum SectionError {
    /// Section not found.
    #[error("section not found")]
    NotFound,

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Service for managing custom sidebar sections.
///
/// Wraps the section repository and enriches sections with unread counts
/// computed from the underlying conversation repositories.
pub struct SectionService {
    section_repo: Arc<SectionRepository>,
    conversation_repo: Arc<UnifiedConversationRepository>,
    pins: Arc<ConversationPins>,
}

impl SectionService {
    /// Create a new SectionService with dependencies.
    pub fn new(
        section_repo: Arc<SectionRepository>,
        conversation_repo: Arc<UnifiedConversationRepository>,
        pins: Arc<ConversationPins>,
    ) -> Self {
        Self {
            section_repo,
            conversation_repo,
            pins,
        }
    }

    /// Create a custom section, optionally filing the conversation that opened the flow.
    pub fn create_section(
        &self,
        name: &str,
        icon: &str,
        conversation: Option<(&str, ConversationType)>,
    ) -> Result<CustomSection, SectionError> {
        let section = self.section_repo.create_section(name, icon)?;
        if let Some((conversation_id, conversation_type)) = conversation {
            if let Err(error) = self.move_conversation_to_section(
                conversation_id,
                conversation_type,
                &section.id,
            ) {
                self.section_repo.delete_section(&section.id)?;
                return Err(error);
            }
        }
        Ok(CustomSection {
            id: section.id,
            name: section.name,
            icon_emoji: section.icon_emoji,
            sort_order: section.sort_order as u32,
            is_expanded: section.is_expanded,
            sort_mode: section.sort_mode,
            conversations: Vec::new(),
            unread_count: 0,
        })
    }

    /// Rename a section.
    pub fn rename_section(&self, id: &SectionId, new_name: &str) -> Result<(), SectionError> {
        let sections = self.section_repo.list_sections()?;
        let section = sections
            .iter()
            .find(|s| s.id == *id)
            .ok_or(SectionError::NotFound)?;

        self.section_repo
            .update_section(id, new_name, &section.icon_emoji, section.sort_order)
            .map_err(|e| match e {
                StorageError::NotFound(_) => SectionError::NotFound,
                other => SectionError::Storage(other),
            })
    }

    /// Change a section's emoji icon.
    pub fn change_icon(&self, id: &SectionId, new_emoji: &str) -> Result<(), SectionError> {
        let sections = self.section_repo.list_sections()?;
        let section = sections
            .iter()
            .find(|s| s.id == *id)
            .ok_or(SectionError::NotFound)?;

        self.section_repo
            .update_section(id, &section.name, new_emoji, section.sort_order)
            .map_err(|e| match e {
                StorageError::NotFound(_) => SectionError::NotFound,
                other => SectionError::Storage(other),
            })
    }

    /// Delete a section. Memberships are cascade-deleted by the database.
    pub fn delete_section(&self, id: &SectionId) -> Result<(), SectionError> {
        self.section_repo.delete_section(id).map_err(|e| match e {
            StorageError::NotFound(_) => SectionError::NotFound,
            other => SectionError::Storage(other),
        })
    }

    /// List all custom sections with their conversations, enriched and ordered.
    ///
    /// Names come from conversation tables, while message stats are read once.
    /// Conversations come back already sorted by the section's `sort_mode`,
    /// pinned first — the UI renders the order it is handed.
    pub fn list_sections_with_conversations(&self) -> Result<Vec<CustomSection>, SectionError> {
        let sections = self.section_repo.list_sections()?;
        if sections.is_empty() {
            return Ok(Vec::new());
        }

        // Names come from conversation tables; unread + last-message come straight
        // from the messages table, which also covers channel-free server DMs —
        // those have no `channels` row, so the unified view cannot emit them and
        // a foldered DM would show no badge at all.
        let names = self.conversation_repo.conversation_names()?;
        let stats = self.conversation_repo.conversation_stats()?;
        let pinned = self.pins.pinned_ids();
        let mut memberships_by_section = self.section_repo.all_section_conversations()?;

        let mut result = Vec::with_capacity(sections.len());
        for section in sections {
            let memberships = memberships_by_section.remove(&section.id).unwrap_or_default();
            let mut unread_count: u32 = 0;

            let mut conversations: Vec<SectionConversation> = memberships
                .into_iter()
                .map(|m| {
                    let key = (m.conversation_id.clone(), m.conversation_type.clone());
                    let stat = stats.get(&key).copied().unwrap_or_default();
                    unread_count += stat.unread_count;
                    SectionConversation {
                        is_pinned: pinned.contains(&m.conversation_id),
                        display_name: names.get(&key).cloned().unwrap_or_default(),
                        unread_count: stat.unread_count,
                        last_message_at: stat.last_message_at.map(|t| t.timestamp_millis()),
                        conversation_id: m.conversation_id,
                        conversation_type: m.conversation_type,
                    }
                })
                .collect();

            sort_conversations(&mut conversations, section.sort_mode);

            result.push(CustomSection {
                id: section.id,
                name: section.name,
                icon_emoji: section.icon_emoji,
                sort_order: section.sort_order as u32,
                is_expanded: section.is_expanded,
                sort_mode: section.sort_mode,
                conversations,
                unread_count,
            });
        }

        Ok(result)
    }

    /// Folder keys for badge visibility; row names and message stats are not
    /// needed when the caller only asks whether a row exists.
    pub fn all_conversation_keys(
        &self,
    ) -> Result<std::collections::HashSet<(String, ConversationType)>, SectionError> {
        Ok(self.section_repo.all_section_conversations()?
            .into_values()
            .flatten()
            .map(|m| (m.conversation_id, m.conversation_type))
            .collect())
    }

    /// Get unified metadata for a specific conversation.
    pub fn get_conversation_metadata(
        &self,
        conversation_id: &str,
        conversation_type: &ConversationType,
    ) -> Result<Option<crate::domain::UnifiedConversation>, SectionError> {
        Ok(self
            .conversation_repo
            .get_conversation(conversation_id, conversation_type)?)
    }

    /// Move a conversation into a section.
    ///
    /// If the conversation is already in another section, it is removed first.
    pub fn move_conversation_to_section(
        &self,
        conversation_id: &str,
        conversation_type: ConversationType,
        section_id: &SectionId,
    ) -> Result<(), SectionError> {
        // Remove from any existing section first
        if let Some(old_section_id) = self
            .section_repo
            .get_conversation_section(conversation_id)?
        {
            self.section_repo
                .remove_from_section(&old_section_id, conversation_id)?;
        }

        self.section_repo
            .add_to_section(section_id, conversation_id, &conversation_type)?;
        Ok(())
    }

    /// Reorder custom sections by writing each id's new `sort_order` to
    /// match its position in `ids`. Unknown ids are skipped. Missing ids
    /// retain their existing sort_order — callers that want "replace the
    /// whole order" should pass every section id they care about.
    pub fn reorder_sections(&self, ids: &[SectionId]) -> Result<(), SectionError> {
        let existing = self.section_repo.list_sections()?;
        for (idx, id) in ids.iter().enumerate() {
            let Some(section) = existing.iter().find(|s| s.id == *id) else {
                continue;
            };
            self.section_repo
                .update_section(id, &section.name, &section.icon_emoji, idx as i32)
                .map_err(|e| match e {
                    StorageError::NotFound(_) => SectionError::NotFound,
                    other => SectionError::Storage(other),
                })?;
        }
        Ok(())
    }

    /// Move one section up (`delta` -1) or down (`delta` +1) in the sidebar.
    ///
    /// This is the caller `reorder_sections` never had. Drag-and-drop does not
    /// exist anywhere in this UI and is a poor first gesture on a phone, so the
    /// folder header menu drives ordering one step at a time.
    /// A move off either end is a no-op, not an error.
    pub fn move_section(&self, id: &SectionId, delta: i32) -> Result<(), SectionError> {
        let sections = self.section_repo.list_sections()?;
        let from = sections
            .iter()
            .position(|s| s.id == *id)
            .ok_or(SectionError::NotFound)?;

        let to = from as i32 + delta;
        if to < 0 || to as usize >= sections.len() {
            return Ok(());
        }

        let mut ids: Vec<SectionId> = sections.iter().map(|s| s.id).collect();
        let moved = ids.remove(from);
        ids.insert(to as usize, moved);
        self.reorder_sections(&ids)
    }

    /// Set how a section orders the conversations inside it.
    pub fn set_sort_mode(&self, id: &SectionId, mode: SectionSortMode) -> Result<(), SectionError> {
        self.section_repo
            .set_sort_mode(id, mode)
            .map_err(|e| match e {
                StorageError::NotFound(_) => SectionError::NotFound,
                other => SectionError::Storage(other),
            })
    }

    /// Toggle the expanded/collapsed state of a section.
    pub fn toggle_expanded(&self, id: &SectionId, expanded: bool) -> Result<(), SectionError> {
        self.section_repo
            .toggle_section_expanded(id, expanded)
            .map_err(|e| match e {
                StorageError::NotFound(_) => SectionError::NotFound,
                other => SectionError::Storage(other),
            })
    }

    /// Check whether a conversation belongs to any custom section.
    pub fn is_in_section(&self, conversation_id: &str) -> Result<bool, SectionError> {
        Ok(self
            .section_repo
            .get_conversation_section(conversation_id)?
            .is_some())
    }

    /// Return the set of all conversation IDs that belong to any section.
    pub fn all_sectioned_conversation_ids(
        &self,
    ) -> Result<std::collections::HashSet<String>, SectionError> {
        Ok(self.section_repo.all_conversation_ids()?)
    }

    /// Remove a conversation from whichever section it belongs to.
    pub fn remove_conversation_from_section(
        &self,
        conversation_id: &str,
    ) -> Result<(), SectionError> {
        if let Some(section_id) = self
            .section_repo
            .get_conversation_section(conversation_id)?
        {
            self.section_repo
                .remove_from_section(&section_id, conversation_id)?;
        }
        Ok(())
    }
}

/// Order one section's conversations. Pinned rows always come first; the mode
/// decides the order within each group.
pub(crate) fn sort_conversations(conversations: &mut [SectionConversation], mode: SectionSortMode) {
    conversations.sort_by(|a, b| {
        b.is_pinned
            .cmp(&a.is_pinned)
            .then_with(|| match mode {
                // Newest first; conversations with no messages sink to the bottom.
                SectionSortMode::Recent => b.last_message_at.cmp(&a.last_message_at),
                SectionSortMode::Unread => (b.unread_count > 0)
                    .cmp(&(a.unread_count > 0))
                    .then_with(|| b.last_message_at.cmp(&a.last_message_at)),
                SectionSortMode::Alphabetical => a
                    .display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase()),
            })
            // Ties resolve by id so the order never flickers between refreshes.
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(
        id: &str,
        name: &str,
        unread: u32,
        at: Option<i64>,
        pinned: bool,
    ) -> SectionConversation {
        SectionConversation {
            conversation_id: id.to_string(),
            conversation_type: ConversationType::Channel,
            display_name: name.to_string(),
            unread_count: unread,
            last_message_at: at,
            is_pinned: pinned,
        }
    }

    fn ids(list: &[SectionConversation]) -> Vec<&str> {
        list.iter().map(|c| c.conversation_id.as_str()).collect()
    }

    #[test]
    fn recent_puts_the_newest_first_and_the_silent_ones_last() {
        let mut list = vec![
            conv("old", "Old", 0, Some(100), false),
            conv("never", "Never", 0, None, false),
            conv("new", "New", 0, Some(900), false),
        ];
        sort_conversations(&mut list, SectionSortMode::Recent);
        assert_eq!(ids(&list), ["new", "old", "never"]);
    }

    #[test]
    fn pinned_beats_every_sort_mode() {
        for mode in [
            SectionSortMode::Recent,
            SectionSortMode::Unread,
            SectionSortMode::Alphabetical,
        ] {
            let mut list = vec![
                conv("zzz", "Zulu", 9, Some(900), false),
                conv("aaa", "Alpha", 0, None, true),
            ];
            sort_conversations(&mut list, mode);
            assert_eq!(ids(&list)[0], "aaa", "pinned must lead in {mode:?}");
        }
    }

    #[test]
    fn unread_mode_floats_unread_then_falls_back_to_recency() {
        let mut list = vec![
            conv("read-new", "A", 0, Some(900), false),
            conv("unread-old", "B", 3, Some(100), false),
            conv("unread-new", "C", 1, Some(800), false),
        ];
        sort_conversations(&mut list, SectionSortMode::Unread);
        assert_eq!(ids(&list), ["unread-new", "unread-old", "read-new"]);
    }

    #[test]
    fn alphabetical_ignores_case() {
        let mut list = vec![
            conv("b", "beta", 0, Some(900), false),
            conv("a", "Alpha", 0, Some(100), false),
        ];
        sort_conversations(&mut list, SectionSortMode::Alphabetical);
        assert_eq!(ids(&list), ["a", "b"]);
    }
}
