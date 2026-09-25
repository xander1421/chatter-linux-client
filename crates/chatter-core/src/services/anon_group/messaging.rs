//! Core messaging operations for anonymous groups.

use crate::domain::anon_group::{AnonGroupId, AnonGroupMessageId};
use crate::domain::ids::ChannelId;
use crate::domain::message::ConversationType;
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
        let channel_id: ChannelId = (*group_id).into();
        let _ = self.message_service.message_repo.update_delivery_status_by_timestamp(
            &channel_id, &ConversationType::AnonGroup, sent_at, "sent",
        );

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
            self.store_message_with_attachments(group_id, &current_user, text, &attachment_ids, sent_at, None)?;

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
        let channel_id: ChannelId = (*group_id).into();
        let _ = self.message_service.message_repo.update_delivery_status_by_timestamp(
            &channel_id, &ConversationType::AnonGroup, sent_at, "sent",
        );

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

        // Look up the original message to build preview text + thumbnail.
        // Falls back to "📷 Photo" / "🎬 Video" / etc. for media-only replies
        // so the receiver sees a non-empty reply bubble on reload.
        let channel_id: ChannelId = (*group_id).into();
        let quoted_msg = self
            .message_service
            .message_repo
            .get_by_conversation_timestamp(&channel_id, reply_to_timestamp)?;
        let (preview_text, quote_attachments) = quoted_msg
            .as_ref()
            .map(|msg| {
                let attachments: Vec<_> = msg
                    .attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect();
                let preview = shared::message_reply_preview(msg, &attachments);
                let qa = shared::build_quoted_attachments(&attachments);
                (preview, qa)
            })
            .unwrap_or_default();

        let quote = shared::build_quote_with_attachments(
            &reply_to_sender.to_string(),
            reply_to_timestamp,
            &preview_text,
            quote_attachments.clone(),
        );
        // Capture thumbnail+mime for local storage so the sender's own bubble
        // renders the preview image without a round-trip through the network.
        let (quote_thumb, quote_mime) = quote_attachments
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));

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

        let quote_ctx = Some(crate::inbox::handlers::base_anon::QuoteContext {
            reply_to: Some(crate::domain::ids::MessageId::from_timestamp(reply_to_timestamp)),
            reply_to_author_id: Some(*reply_to_sender),
            reply_to_text: (!preview_text.is_empty()).then(|| preview_text.clone()),
            reply_to_thumbnail: quote_thumb,
            reply_to_thumbnail_mime: quote_mime,
        });

        let message_id = self.store_message_with_content_json(
            group_id,
            &current_user,
            shared::CONTENT_TYPE_MIXED,
            &content_json,
            &attachment_ids,
            sent_at,
            quote_ctx,
        )?;

        // NDSS '21 decorrelation: delay anonymous mailbox send after CDN upload
        if !attachment_ids.is_empty() {
            crate::mailbox::timing::TimingObfuscation::with_defaults()
                .delay_for_attachment_decorrelation()
                .await;
        }

        self.send_message(group_id, inner_content).await?;

        // Mark as "sent" for delivery status indicator
        // channel_id already bound above at line 108 from (*group_id).into()
        let _ = self.message_service.message_repo.update_delivery_status_by_timestamp(
            &channel_id, &ConversationType::AnonGroup, sent_at, "sent",
        );

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
        use prost::Message as ProstMessage;

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
        // Get pending members (mailbox_pk not yet confirmed). Messages for
        // these members are enqueued and drained when their JoinAck lands.
        let pending_members = self.anon_group_repo.get_members_without_mailbox(group_id)?;

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

        // A user's post is parked per member when that member's send fails,
        // and retried by `retry_parked_sends`: a first post spends one
        // pre-fetched token per member without a session, so a post to more
        // new members than the batch holds fails for the overflow until the
        // next refill. Call signaling is not parked — a late ring is worse
        // than none.
        let parkable = undelivered_post(&content).is_some();
        let mut sent_count = 0;
        let mut parked_count = 0;
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
            match self
                .send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone())
                .await
            {
                Ok(()) => sent_count += 1,
                Err(e) if parkable => {
                    tracing::info!("Send to member {} failed ({}); parked for retry", member.user_id, e);
                    match self.anon_group_repo.enqueue_pending_message(
                        group_id,
                        &member.user_id,
                        &content.encode_to_vec(),
                    ) {
                        Ok(()) => parked_count += 1,
                        Err(e) => tracing::warn!("Failed to park send to member {}: {}", member.user_id, e),
                    }
                }
                Err(e) => tracing::warn!("Failed to send to member {}: {}", member.user_id, e),
            }
        }

        // Queue the same Content for members whose JoinAck hasn't been
        // received yet. Encode once and reuse; drained in handle_join_ack.
        let mut queued_count = 0usize;
        if !pending_members.is_empty() {
            let content_blob = content.encode_to_vec();
            for member in &pending_members {
                if member.user_id == current_user {
                    continue;
                }
                if let Err(e) = self.anon_group_repo.enqueue_pending_message(
                    group_id,
                    &member.user_id,
                    &content_blob,
                ) {
                    tracing::warn!(
                        group_id = %group_id,
                        recipient = %member.user_id,
                        error = %e,
                        "Failed to enqueue pending anon group message"
                    );
                } else {
                    queued_count += 1;
                }
            }
        }

        tracing::debug!(
            "Sent message to group {} ({} members, {} parked, {} queued for pending joiners)",
            group_id,
            sent_count,
            parked_count,
            queued_count,
        );
        Ok(())
    }

    /// Drain any pending outbound anon group messages for a member whose
    /// mailbox_pk just became available (e.g. after their JoinAck arrives).
    ///
    /// Each row is re-decoded into a `Content` proto and delivered via
    /// `send_to_anon_mailbox`. Rows are deleted on success, left in place on
    /// failure for retry on the next JoinAck or `retry_parked_sends`. A row
    /// that fails `MAX_PARKED_ATTEMPTS` times is given up and, if it was a
    /// user's post, reported in the chat as `SystemMessage::Undelivered`. A
    /// failure because the token batch is empty is not the member's fault
    /// and does not count.
    pub(crate) async fn drain_pending_messages(
        &self,
        group_id: &AnonGroupId,
        recipient_user_id: &crate::domain::UserId,
        recipient_mailbox_pk: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        use prost::Message as ProstMessage;

        let pending = self
            .anon_group_repo
            .get_pending_messages_for_member(group_id, recipient_user_id)?;

        if pending.is_empty() {
            return Ok(());
        }

        tracing::info!(
            group_id = %group_id,
            recipient = %recipient_user_id,
            count = pending.len(),
            "Draining pending anon group messages after JoinAck"
        );

        for (row_id, blob, attempts) in pending {
            let content = match Content::decode(blob.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        row_id,
                        error = %e,
                        "Pending anon group message blob failed to decode; dropping"
                    );
                    // Undecodable row is poison — remove it to avoid re-trying forever.
                    let _ = self.anon_group_repo.delete_pending_message(row_id);
                    continue;
                }
            };

            match self
                .send_to_anon_mailbox(recipient_user_id, recipient_mailbox_pk, content.clone())
                .await
            {
                Ok(()) => {
                    if let Err(e) = self.anon_group_repo.delete_pending_message(row_id) {
                        tracing::warn!(
                            row_id,
                            error = %e,
                            "Failed to delete drained pending anon group message row"
                        );
                    }
                }
                Err(e) => {
                    // Stop draining on first failure so messages stay in order.
                    if self.token_manager.available_token_count().unwrap_or(0) == 0 {
                        tracing::info!(row_id, "Pending anon group message waits for the token refill");
                        break;
                    }
                    if attempts + 1 < MAX_PARKED_ATTEMPTS {
                        tracing::warn!(
                            row_id,
                            recipient = %recipient_user_id,
                            error = %e,
                            "Failed to drain pending anon group message; leaving for retry"
                        );
                        let _ = self
                            .anon_group_repo
                            .increment_pending_message_attempts(row_id);
                        break;
                    }
                    tracing::warn!(
                        row_id,
                        recipient = %recipient_user_id,
                        error = %e,
                        "Giving up on pending anon group message"
                    );
                    self.anon_group_repo.delete_pending_message(row_id)?;
                    if let Some(undelivered) = undelivered_post(&content) {
                        self.store_system_message(
                            group_id,
                            Some(*recipient_user_id),
                            undelivered,
                            TimestampNanos::now().as_u64(),
                        )?;
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// Retry every parked send to a member whose mailbox is known.
    ///
    /// Driven by its own jittered loop (`CoreServices::spawn_token_refill`),
    /// not by the refill itself, so the anonymous sends that spend the new
    /// tokens are not adjacent in time to the authenticated request that
    /// fetched them (NDSS'21 §VI-F). Stops early when the batch is empty.
    pub async fn retry_parked_sends(&self) -> Result<(), AnonGroupError> {
        for group in self.anon_group_repo.list_active_groups()? {
            for member in self.anon_group_repo.get_members_with_mailbox(&group.id)? {
                if self.token_manager.available_token_count().unwrap_or(0) == 0 {
                    return Ok(());
                }
                let mailbox_pk = member
                    .mailbox_pk
                    .expect("get_members_with_mailbox guarantees mailbox_pk");
                self.drain_pending_messages(&group.id, &member.user_id, &mailbox_pk)
                    .await?;
            }
        }
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

/// Failed attempts after which a parked send is given up.
const MAX_PARKED_ATTEMPTS: u32 = 5;

/// The `Undelivered` notice for a parked `Content`, if it was a user's post.
fn undelivered_post(content: &Content) -> Option<crate::domain::message::SystemMessage> {
    let Some(content_v1::content::Content::AnonGroupMessage(m)) = &content.content else {
        return None;
    };
    let Some(content_v1::content::Content::DataMessage(dm)) =
        m.inner_content.as_deref().and_then(|c| c.content.as_ref())
    else {
        return None;
    };
    let preview = if dm.body.is_empty() && !dm.attachments.is_empty() {
        "📎 Attachment".to_string()
    } else {
        crate::domain::message::truncate_preview_text(&dm.body, 40)
    };
    Some(crate::domain::message::SystemMessage::Undelivered { sent_at: dm.timestamp, preview })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::SystemMessage;
    use crate::proto::content_v1::AnonGroupMessage;

    fn wrap(inner: Content) -> Content {
        Content {
            content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(
                AnonGroupMessage { group_id: vec![0; 16], inner_content: Some(Box::new(inner)) },
            ))),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_users_post_is_parked_and_reported() {
        let long = "x".repeat(100);
        match undelivered_post(&wrap(build_text_content(&long, 42))) {
            Some(SystemMessage::Undelivered { sent_at, preview }) => {
                assert_eq!(sent_at, 42);
                assert_eq!(preview.chars().count(), 40);
                assert!(preview.ends_with("..."));
            }
            other => panic!("expected Undelivered, got {other:?}"),
        }

        let attachment_only = build_data_message_content("", vec![Default::default()], 7);
        assert!(matches!(
            undelivered_post(&wrap(attachment_only)),
            Some(SystemMessage::Undelivered { preview, .. }) if preview == "📎 Attachment"
        ));

        // Call signaling is never parked: a late ring is worse than none.
        let hangup = Content {
            content: Some(content_v1::content::Content::AnonCallHangup(Default::default())),
            ..Default::default()
        };
        assert!(undelivered_post(&wrap(hangup)).is_none());
    }
}
