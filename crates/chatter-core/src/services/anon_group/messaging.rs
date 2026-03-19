//! Core messaging operations for anonymous groups.

use crate::domain::anon_group::{AnonGroupId, AnonGroupMessageId};
use crate::proto::content_v1::{self, Content};
use crate::services::gateway::builders::{
    build_text_content, build_data_message_content,
    build_data_message_content_with_quote,
};
use crate::services::gateway::shared;
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
    ) -> Result<(AnonGroupMessageId, u64), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Create timestamp once - used for both local storage and sending
        let sent_at = TimestampNanos::now().as_u64();

        // Create DataMessage content
        let inner_content = build_text_content(text, sent_at);

        // Store message locally first with the same timestamp we'll send
        let message_id = self.store_text_message(group_id, &current_user, text, sent_at)?;

        // Send to all members
        self.send_message(group_id, inner_content).await?;

        // Mark as "sent" for delivery status indicator
        let _ = self.anon_group_repo.update_delivery_status(&message_id, "sent");

        Ok((message_id, sent_at))
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
    ) -> Result<(AnonGroupMessageId, u64), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Extract attachment IDs from pointers before moving them
        let attachment_ids = shared::extract_attachment_ids(&attachments);

        // Create timestamp once - used for both local storage and sending
        let sent_at = TimestampNanos::now().as_u64();

        // Create DataMessage content with attachments
        let inner_content = build_data_message_content(text, attachments, sent_at);

        // Store message locally first with the same timestamp we'll send
        let message_id =
            self.store_message_with_attachments(group_id, &current_user, text, &attachment_ids, sent_at)?;

        // NDSS '21 decorrelation: delay anonymous mailbox send after CDN upload
        // so the server can't correlate the authenticated upload with this write.
        // Message is already visible in UI (optimistic insert above).
        if !attachment_ids.is_empty() {
            crate::mailbox::timing::TimingObfuscation::with_defaults()
                .delay_for_attachment_decorrelation()
                .await;
        }

        // Send to all members
        self.send_message(group_id, inner_content).await?;

        // Mark as "sent" for delivery status indicator
        let _ = self.anon_group_repo.update_delivery_status(&message_id, "sent");

        Ok((message_id, sent_at))
    }

    /// Send a reply to a message in the group.
    ///
    /// Constructs a Quote proto from the original message and sends it
    /// as part of the DataMessage. Also stores locally with quote metadata.
    pub async fn send_reply(
        &self,
        group_id: &AnonGroupId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
        reply_to_sender: &crate::domain::UserId,
        reply_to_timestamp: u64,
    ) -> Result<(AnonGroupMessageId, u64), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Look up the original message to get preview text
        let preview_text = self
            .anon_group_repo
            .get_message_by_timestamp(group_id, reply_to_timestamp)?
            .map(|msg| shared::truncate_preview(&msg.as_text().unwrap_or_default()))
            .unwrap_or_default();

        let quote = shared::build_quote(
            &reply_to_sender.to_string(),
            reply_to_timestamp,
            &preview_text,
        );

        // Extract attachment IDs
        let attachment_ids = shared::extract_attachment_ids(&attachments);

        let sent_at = TimestampNanos::now().as_u64();

        let inner_content = build_data_message_content_with_quote(text, attachments, Some(quote), sent_at);

        // Resolve author display name for storage
        let author_display_name = self
            .get_member_display_name(group_id, reply_to_sender)
            .ok()
            .flatten()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| reply_to_sender.to_string());

        // Store locally with quote metadata in content_json
        let content_json = shared::build_reply_content_json(
            text,
            &reply_to_sender.to_string(),
            &author_display_name,
            reply_to_timestamp,
            &preview_text,
        );

        let message_id = self.store_message_with_content_json(
            group_id,
            &current_user,
            shared::CONTENT_TYPE_MIXED,
            &content_json,
            &attachment_ids,
            sent_at,
        )?;

        // NDSS '21 decorrelation: delay anonymous mailbox send after CDN upload
        if !attachment_ids.is_empty() {
            crate::mailbox::timing::TimingObfuscation::with_defaults()
                .delay_for_attachment_decorrelation()
                .await;
        }

        self.send_message(group_id, inner_content).await?;

        // Mark as "sent" for delivery status indicator
        let _ = self.anon_group_repo.update_delivery_status(&message_id, "sent");

        Ok((message_id, sent_at))
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

    /// Send an ephemeral message (reaction, typing, receipt) to all group members.
    ///
    /// Unlike `send_message`, this skips rate limiting since ephemeral messages
    /// are lightweight and shouldn't count against the message rate limit.
    pub async fn send_ephemeral_message(
        &self,
        group_id: &AnonGroupId,
        inner_content: Content,
    ) -> Result<(), AnonGroupError> {
        use crate::proto::content_v1::AnonGroupMessage;

        let current_user = self.current_user_id()?;

        // Check if group is blocked (cannot send to blocked groups)
        self.check_group_not_blocked(group_id)?;

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
        for member in &members {
            if member.user_id == current_user {
                continue;
            }

            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            if let Err(e) = self
                .send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone())
                .await
            {
                tracing::warn!("Failed to send ephemeral to member {}: {}", member.user_id, e);
            } else {
                sent_count += 1;
            }
        }

        tracing::debug!(
            "Sent ephemeral message to group {} ({} members)",
            group_id,
            sent_count
        );
        Ok(())
    }
}
