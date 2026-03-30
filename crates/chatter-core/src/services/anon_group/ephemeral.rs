//! Ephemeral message operations (reactions, typing, delete, pin, edit).

use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::ChannelId;
use crate::domain::message::ConversationType;
use crate::services::{
    build_delete_for_everyone_content, build_edit_content, build_pin_content,
    build_reaction_content, build_typing_content,
};

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Send a reaction to a message in the group.
    ///
    /// Uses the ephemeral message path (no rate limiting) since reactions
    /// are lightweight and shouldn't count against the message rate limit.
    pub async fn send_reaction(
        &self,
        group_id: &AnonGroupId,
        emoji: &str,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_reaction_content(emoji, remove, target_author_aci, target_sent_timestamp);
        self.send_ephemeral_message(group_id, content).await
    }

    /// Send a typing indicator to all group members.
    pub async fn send_typing_indicator(
        &self,
        group_id: &AnonGroupId,
        started: bool,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_typing_content(started);
        self.send_ephemeral_message(group_id, content).await
    }

    /// Send delete-for-everyone to all group members.
    pub async fn send_delete_for_everyone(
        &self,
        group_id: &AnonGroupId,
        target_sent_timestamp: u64,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_delete_for_everyone_content(target_sent_timestamp);
        self.send_ephemeral_message(group_id, content).await
    }

    /// Send pin/unpin notification to all group members.
    pub async fn send_pin(
        &self,
        group_id: &AnonGroupId,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        pin: bool,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_pin_content(target_author_aci, target_sent_timestamp, pin);
        self.send_ephemeral_message(group_id, content).await
    }

    /// Send an edit message to update a previously sent message.
    ///
    /// This sends an edit notification to all group members and updates the message locally.
    pub async fn send_edit(
        &self,
        group_id: &AnonGroupId,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> Result<(), AnonGroupError> {
        let content = build_edit_content(target_sent_timestamp, new_text);
        self.send_message(group_id, content).await?;

        // Update message locally
        let channel_id: ChannelId = (*group_id).into();
        self.message_service
            .message_repo
            .update_text_by_timestamp(&channel_id, &ConversationType::AnonGroup, target_sent_timestamp, new_text)?;

        Ok(())
    }
}
