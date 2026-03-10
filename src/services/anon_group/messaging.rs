//! Core messaging operations for anonymous groups.

use crate::domain::anon_group::{AnonGroupId, AnonGroupMessageId};
use crate::domain::AttachmentId;
use crate::proto::content_v1::{self, Content, DataMessage};
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Send a text message to all members of an anonymous group.
    ///
    /// This is a convenience wrapper around `send_message` that creates a
    /// DataMessage Content with the given text.
    ///
    /// Returns the stored message ID.
    pub async fn send_text(
        &self,
        group_id: &AnonGroupId,
        text: &str,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Create timestamp once - used for both local storage and sending
        let sent_at = TimestampNanos::now().as_u64();

        // Create DataMessage content
        let data_message = DataMessage {
            body: text.to_string(),
            ..Default::default()
        };
        let inner_content = Content {
            content: Some(content_v1::content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: sent_at,
            padding: Vec::new(),
        };

        // Store message locally first with the same timestamp we'll send
        let message_id = self.store_text_message(group_id, &current_user, text, sent_at)?;

        // Send to all members
        self.send_message(group_id, inner_content).await?;

        Ok(message_id)
    }

    /// Send a text message with attachments to an anonymous group.
    ///
    /// Similar to `send_text` but also includes attachment pointers.
    /// Attachments should already be uploaded to CDN before calling this.
    ///
    /// Returns the stored message ID.
    pub async fn send_with_attachments(
        &self,
        group_id: &AnonGroupId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Extract attachment IDs from pointers before moving them
        let attachment_ids: Vec<AttachmentId> = attachments
            .iter()
            .filter_map(|ptr| {
                if ptr.cdn_key.is_empty() {
                    None
                } else {
                    Some(AttachmentId::from_cdn_key(&ptr.cdn_key))
                }
            })
            .collect();

        // Create timestamp once - used for both local storage and sending
        let sent_at = TimestampNanos::now().as_u64();

        // Create DataMessage content with attachments
        let data_message = DataMessage {
            body: text.to_string(),
            attachments,
            ..Default::default()
        };
        let inner_content = Content {
            content: Some(content_v1::content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: sent_at,
            padding: Vec::new(),
        };

        // Store message locally first with the same timestamp we'll send
        let message_id =
            self.store_message_with_attachments(group_id, &current_user, text, &attachment_ids, sent_at)?;

        // Send to all members
        self.send_message(group_id, inner_content).await?;

        tracing::info!(
            group_id = %group_id,
            text_len = text.len(),
            attachment_count = attachment_ids.len(),
            "Sent group message with attachments"
        );

        Ok(message_id)
    }

    /// Send a message to all members of an anonymous group.
    ///
    /// The message is wrapped in AnonGroupMessage and sent to each member's
    /// anonymous mailbox via pairwise sealed sender.
    ///
    /// # Errors
    ///
    /// - `GroupBlocked` if the group is blocked
    /// - `RateLimitExceeded` if too many messages sent recently
    pub async fn send_message(
        &self,
        group_id: &AnonGroupId,
        inner_content: Content,
    ) -> Result<(), AnonGroupError> {
        use crate::proto::content_v1::AnonGroupMessage;

        let current_user = self.current_user_id()?;

        // Check if group is blocked (cannot send to blocked groups)
        self.check_group_not_blocked(group_id)?;

        // Check rate limit before sending
        self.check_rate_limit(group_id)?;

        // Verify group exists and we're a member
        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        // Get confirmed members (those with mailbox keys)
        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Wrap the content in AnonGroupMessage
        let group_message = AnonGroupMessage {
            group_id: group_id.to_vec(),
            inner_content: Some(Box::new(inner_content)),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(
                group_message,
            ))),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        let mut sent_count = 0;
        // Fan out to each member's anonymous mailbox
        for member in &members {
            if member.user_id == current_user {
                continue; // Don't send to ourselves
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            // Send to member's anonymous group mailbox (not DM)
            if let Err(e) = self
                .send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone())
                .await
            {
                tracing::warn!("Failed to send to member {}: {}", member.user_id, e);
            } else {
                sent_count += 1;
            }
        }

        tracing::debug!(
            "Sent message to group {} ({} members)",
            group_id,
            sent_count
        );
        Ok(())
    }
}
