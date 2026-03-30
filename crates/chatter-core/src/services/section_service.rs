//! Section service.
//!
//! Manage custom sidebar sections and their conversation memberships.

use std::sync::Arc;

use crate::domain::ids::SectionId;
use crate::domain::message::ConversationType;
use crate::domain::section::{CustomSection, SectionConversation};
use crate::error::StorageError;
use crate::storage::repositories::anon_dm_repo::AnonDmRepository;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::section_repo::SectionRepository;

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
    channel_repo: Arc<ChannelRepository>,
    anon_dm_repo: Arc<AnonDmRepository>,
}

impl SectionService {
    /// Create a new SectionService with dependencies.
    pub fn new(
        section_repo: Arc<SectionRepository>,
        channel_repo: Arc<ChannelRepository>,
        anon_dm_repo: Arc<AnonDmRepository>,
    ) -> Self {
        Self {
            section_repo,
            channel_repo,
            anon_dm_repo,
        }
    }

    /// Create a new custom section.
    pub fn create_section(&self, name: &str, icon: &str) -> Result<CustomSection, SectionError> {
        let section = self.section_repo.create_section(name, icon)?;
        Ok(CustomSection {
            id: section.id,
            name: section.name,
            icon_emoji: section.icon_emoji,
            sort_order: section.sort_order as u32,
            is_expanded: section.is_expanded,
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

    /// List all sections with their conversations and computed unread counts.
    pub fn list_sections_with_conversations(&self) -> Result<Vec<CustomSection>, SectionError> {
        let sections = self.section_repo.list_sections()?;
        let mut result = Vec::with_capacity(sections.len());

        for section in sections {
            let memberships = self.section_repo.get_section_conversations(&section.id)?;
            let mut unread_count: u32 = 0;

            let conversations: Vec<SectionConversation> = memberships
                .into_iter()
                .map(|m| {
                    unread_count += self.get_conversation_unread(
                        &m.conversation_id,
                        &m.conversation_type,
                    );
                    SectionConversation {
                        conversation_id: m.conversation_id,
                        conversation_type: m.conversation_type,
                    }
                })
                .collect();

            result.push(CustomSection {
                id: section.id,
                name: section.name,
                icon_emoji: section.icon_emoji,
                sort_order: section.sort_order as u32,
                is_expanded: section.is_expanded,
                conversations,
                unread_count,
            });
        }

        Ok(result)
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
        if let Some(old_section_id) = self.section_repo.get_conversation_section(conversation_id)? {
            self.section_repo
                .remove_from_section(&old_section_id, conversation_id)?;
        }

        self.section_repo
            .add_to_section(section_id, conversation_id, &conversation_type)?;
        Ok(())
    }

    /// Remove a conversation from whichever section it belongs to.
    pub fn remove_conversation_from_section(
        &self,
        conversation_id: &str,
    ) -> Result<(), SectionError> {
        if let Some(section_id) = self.section_repo.get_conversation_section(conversation_id)? {
            self.section_repo
                .remove_from_section(&section_id, conversation_id)?;
        }
        Ok(())
    }

    /// Look up the unread count for a conversation by type.
    fn get_conversation_unread(
        &self,
        conversation_id: &str,
        conversation_type: &ConversationType,
    ) -> u32 {
        match conversation_type {
            ConversationType::Channel
            | ConversationType::ServerDm
            | ConversationType::MailboxDm
            | ConversationType::SavedMessages => {
                if let Ok(id) = conversation_id.parse() {
                    if let Ok(Some(channel)) = self.channel_repo.get(&id) {
                        return channel.unread_count;
                    }
                }
                0
            }
            ConversationType::AnonDm => {
                if let Ok(id) = conversation_id.parse() {
                    if let Ok(Some(dm)) = self.anon_dm_repo.get_conversation(&id) {
                        return dm.unread_count;
                    }
                }
                0
            }
            ConversationType::AnonGroup => {
                // Anon groups don't track unread counts in their domain type
                0
            }
        }
    }
}
