//! Ephemeral message operations (reactions, typing, delete, pin, edit).

use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::ConversationType;
use crate::services::{build_edit_content, build_pin_content, build_typing_content};

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
        let actor = self.current_user_id()?;
        let action_ts = self
            .message_service
            .message_repo
            .allocate_reaction_clock(&actor)?;
        self.send_reaction_at(
            group_id,
            emoji,
            target_author_aci,
            target_sent_timestamp,
            remove,
            action_ts,
        )
        .await
    }

    /// Send an already allocated action without restamping it.
    pub async fn send_reaction_at(
        &self,
        group_id: &AnonGroupId,
        emoji: &str,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        remove: bool,
        action_ts: u64,
    ) -> Result<(), AnonGroupError> {
        self.check_group_not_blocked(group_id)?;

        let content = crate::services::gateway::builders::build_reaction_content_with_channel_at(
            emoji,
            remove,
            target_author_aci,
            target_sent_timestamp,
            None,
            action_ts,
        );
        self.send_reaction_content(group_id, content).await
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

        let author = self.current_user_id()?;
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            target_sent_timestamp,
            Some(&author.to_string()),
            None,
        );
        self.send_ephemeral_message(group_id, content).await
    }

    pub async fn send_targeted_delete_for_everyone(
        &self,
        group_id: &AnonGroupId,
        author: &UserId,
        timestamp: u64,
    ) -> Result<(), AnonGroupError> {
        if *author != self.current_user_id()? {
            return Err(AnonGroupError::InvalidData(
                "Group deletes must target our own author".into(),
            ));
        }
        let content = crate::services::gateway::builders::build_targeted_delete_content(
            timestamp,
            Some(&author.to_string()),
            None,
        );
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
        self.message_service.message_repo.update_text_by_timestamp(
            &channel_id,
            &ConversationType::AnonGroup,
            target_sent_timestamp,
            new_text,
        )?;

        Ok(())
    }
}
