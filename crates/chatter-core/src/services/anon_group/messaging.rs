//! Core messaging operations for anonymous groups.

use crate::domain::anon_group::{AnonGroupId, AnonGroupMessageId};
use crate::domain::ids::ChannelId;
use crate::proto::content_v1::{self, Content};
use crate::services::gateway::builders::{
    build_data_message_content, build_data_message_content_with_quote, build_text_content,
};
use crate::services::gateway::shared;
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    fn retain_group_row(
        &self,
        group_id: &AnonGroupId,
        owner: &crate::domain::UserId,
        inner: &Content,
        id: &AnonGroupMessageId,
    ) -> Result<(), AnonGroupError> {
        // This freshly inserted row has no remote submission yet. Make its
        // optimistic status truthful before any decorrelation wait or queue.
        self.message_service
            .database
            .connection()?
            .execute(
                "UPDATE messages SET delivery_status='sending',failure_reason=NULL WHERE id=?1",
                rusqlite::params![id.as_str()],
            )
            .map_err(crate::error::StorageError::from)?;
        let key = super::send_ledger::key(&owner.to_string(), &group_id.to_string(), inner);
        super::send_ledger::register(&self.message_service.database, &key, id.as_str())?;
        crate::services::gateway::submission::bind_original_row(id.as_str().to_owned())?;
        Ok(())
    }

    fn record_group_delivery(
        &self,
        group_id: &AnonGroupId,
        content: &Content,
        recipient: &crate::domain::UserId,
        status: &str,
    ) -> Result<(), AnonGroupError> {
        let Some(content_v1::content::Content::AnonGroupMessage(message)) =
            content.content.as_ref()
        else {
            return Ok(());
        };
        let Some(inner) = message.inner_content.as_deref() else {
            return Ok(());
        };
        let owner = self.current_user_id()?;
        let key = super::send_ledger::key(&owner.to_string(), &group_id.to_string(), inner);
        if let Some(id) = super::send_ledger::record(
            &self.message_service.database,
            &key,
            &recipient.to_string(),
            status,
        )? {
            self.message_service
                .finish_retry_row(&id, "sent", None)
                .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;
        }
        Ok(())
    }

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

        self.retain_group_row(group_id, &current_user, &inner_content, &message_id)?;
        // Send to all members
        // Persist unconfirmed before any deposit; an error or cancellation is
        // not evidence that all recipients received nothing.
        match self.send_message(group_id, inner_content).await {
            Ok(()) => {}
            Err(AnonGroupError::DeliveryUnconfirmed) => return Ok((message_id, sent_at)),
            Err(error) => return Err(error),
        }

        // Mark as "sent" for delivery status indicator
        let id = message_id
            .as_str()
            .parse()
            .map_err(|_| AnonGroupError::InvalidData("Invalid original message ID".into()))?;
        self.message_service
            .finish_retry_row(&id, "sent", None)
            .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;

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
        let message_id = self.store_message_with_attachments(
            group_id,
            &current_user,
            text,
            &attachment_ids,
            sent_at,
            None,
        )?;

        self.retain_group_row(group_id, &current_user, &inner_content, &message_id)?;
        // NDSS '21 decorrelation: delay anonymous mailbox send after CDN upload
        // so the server can't correlate the authenticated upload with this write.
        // Message is already visible in UI (optimistic insert above).
        if !attachment_ids.is_empty() {
            crate::mailbox::timing::TimingObfuscation::with_defaults()
                .delay_for_attachment_decorrelation()
                .await;
        }

        // Send to all members
        // Persist unconfirmed before any deposit; an error or cancellation is
        // not evidence that all recipients received nothing.
        match self.send_message(group_id, inner_content).await {
            Ok(()) => {}
            Err(AnonGroupError::DeliveryUnconfirmed) => return Ok((message_id, sent_at)),
            Err(error) => return Err(error),
        }

        // Mark as "sent" for delivery status indicator
        let id = message_id
            .as_str()
            .parse()
            .map_err(|_| AnonGroupError::InvalidData("Invalid original message ID".into()))?;
        self.message_service
            .finish_retry_row(&id, "sent", None)
            .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;

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

        let inner_content =
            build_data_message_content_with_quote(text, attachments, Some(quote), sent_at);

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
            reply_to: Some(crate::domain::ids::MessageId::from_timestamp(
                reply_to_timestamp,
            )),
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

        self.retain_group_row(group_id, &current_user, &inner_content, &message_id)?;
        // NDSS '21 decorrelation: delay anonymous mailbox send after CDN upload
        if !attachment_ids.is_empty() {
            crate::mailbox::timing::TimingObfuscation::with_defaults()
                .delay_for_attachment_decorrelation()
                .await;
        }

        // Persist unconfirmed before any deposit; an error or cancellation is
        // not evidence that all recipients received nothing.
        match self.send_message(group_id, inner_content).await {
            Ok(()) => {}
            Err(AnonGroupError::DeliveryUnconfirmed) => return Ok((message_id, sent_at)),
            Err(error) => return Err(error),
        }

        // Mark as "sent" for delivery status indicator
        let id = message_id
            .as_str()
            .parse()
            .map_err(|_| AnonGroupError::InvalidData("Invalid original message ID".into()))?;
        self.message_service
            .finish_retry_row(&id, "sent", None)
            .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;

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

        let ledger_key = super::send_ledger::key(
            &current_user.to_string(),
            &group_id.to_string(),
            &inner_content,
        );
        let recipients: Vec<String> = members
            .iter()
            .map(|m| m.user_id)
            .chain(pending_members.iter().map(|m| m.user_id))
            .filter(|id| *id != current_user)
            .map(|id| id.to_string())
            .collect();
        if super::send_ledger::initialize(&self.message_service.database, &ledger_key, &recipients)?
            == Some(false)
        {
            crate::services::gateway::submission::mark_existing_submission();
            return Err(AnonGroupError::DeliveryUnconfirmed);
        }

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
        let mut unconfirmed = false;
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
                .send_to_anon_mailbox(
                    &member.user_id,
                    member.device_id,
                    &mailbox_pk,
                    content.clone(),
                )
                .await
            {
                Ok(()) => {
                    sent_count += 1;
                    self.record_group_delivery(group_id, &content, &member.user_id, "confirmed")?;
                }
                Err(AnonGroupError::DeliveryUnconfirmed) => {
                    unconfirmed = true;
                    self.record_group_delivery(group_id, &content, &member.user_id, "unknown")?;
                }
                Err(e) if parkable => {
                    // An expired originating session must not park old content.
                    crate::services::gateway::submission::mark_submission_started()?;
                    tracing::info!("Send to member failed ({}); parked for retry", e);
                    super::send_ledger::enqueue(
                        &self.message_service.database,
                        &self.anon_group_repo,
                        &ledger_key,
                        group_id,
                        &member.user_id,
                        &content.encode_to_vec(),
                    )?;
                    parked_count += 1;
                }
                Err(e) => {
                    self.record_group_delivery(group_id, &content, &member.user_id, "failed")?;
                    return Err(e);
                }
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
                crate::services::gateway::submission::mark_submission_started()?;
                super::send_ledger::enqueue(
                    &self.message_service.database,
                    &self.anon_group_repo,
                    &ledger_key,
                    group_id,
                    &member.user_id,
                    &content_blob,
                )?;
                queued_count += 1;
            }
        }

        tracing::debug!(
            "Sent message to group ({} members, {} parked, {} queued for pending joiners)",
            sent_count,
            parked_count,
            queued_count
        );
        if unconfirmed || parked_count > 0 || queued_count > 0 {
            return Err(AnonGroupError::DeliveryUnconfirmed);
        }
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

            let device = self
                .anon_group_repo
                .get_member(group_id, recipient_user_id)?
                .and_then(|m| m.device_id);
            match self
                .send_to_anon_mailbox(
                    recipient_user_id,
                    device,
                    recipient_mailbox_pk,
                    content.clone(),
                )
                .await
            {
                Ok(()) => {
                    // Persist the exact recipient receipt before deleting the
                    // queue row. A deletion failure cannot trigger re-encryption.
                    self.record_group_delivery(group_id, &content, recipient_user_id, "confirmed")?;
                    self.anon_group_repo.delete_pending_message(row_id)?;
                }
                Err(AnonGroupError::DeliveryUnconfirmed) => {
                    // Keep the row and exact encrypted submission. Do not
                    // increment a failure ceiling or claim it was undelivered.
                    break;
                }
                Err(e) => {
                    // Stop draining on first failure so messages stay in order.
                    if self.token_manager.available_token_count().unwrap_or(0) == 0 {
                        tracing::info!(
                            row_id,
                            "Pending anon group message waits for the token refill"
                        );
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
                    self.record_group_delivery(group_id, &content, recipient_user_id, "failed")?;
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
        let current = self.current_user_id()?;
        for group in self.anon_group_repo.list_active_groups()? {
            // Before the first authoritative roster, the inviter is deliberately
            // stored as Member. Reannounce only this incomplete bootstrap, using
            // the existing mailbox and authenticated JoinAck protocol.
            if !group.is_blocked() && group.owner_id != current {
                let own = self.anon_group_repo.get_member(&group.id, &current)?;
                let owner = self
                    .anon_group_repo
                    .get_member(&group.id, &group.owner_id)?;
                let mailbox = self.anon_group_repo.get_mailbox(&group.id)?;
                if let (Some(own), Some(owner), Some(mailbox)) = (own, owner, mailbox) {
                    if owner.role == crate::domain::anon_group::AnonGroupRole::Member
                        && own.mailbox_pk == Some(mailbox.public_key)
                        && !mailbox.is_expired()
                        && self.token_manager.available_token_count().unwrap_or(0) > 0
                    {
                        if let Some(owner_key) = owner.mailbox_pk {
                            if self
                                .anon_group_repo
                                .claim_bootstrap_reannouncement(&group.id, &current)?
                            {
                                if let Err(error) = self
                                    .send_join_ack(
                                        &group.owner_id,
                                        group.id,
                                        &owner_key,
                                        &mailbox.public_key,
                                    )
                                    .await
                                {
                                    tracing::warn!(%error, "Bootstrap reannouncement failed; bounded retry retained");
                                }
                            }
                        }
                    }
                }
            }
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

    /// Reaction-only outcome policy: at least one confirmed remote delivery is
    /// success, including partial fanout. Never resend or re-toggle automatically.
    pub(super) async fn send_reaction_content(
        &self,
        group_id: &AnonGroupId,
        inner_content: Content,
    ) -> Result<(), AnonGroupError> {
        dispatch_reaction_content(
            group_id,
            inner_content,
            || {
                let current = self.current_user_id()?;
                self.check_group_not_blocked(group_id)?;
                let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;
                Ok(members
                    .into_iter()
                    .filter(|m| m.user_id != current)
                    .filter_map(|m| m.mailbox_pk.map(|key| (m.user_id, m.device_id, key)))
                    .collect())
            },
            |(peer, device, key), content| async move {
                self.send_to_anon_mailbox(&peer, device, &key, content)
                    .await
            },
        )
        .await
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
                .send_to_anon_mailbox(
                    &member.user_id,
                    member.device_id,
                    &mailbox_pk,
                    content.clone(),
                )
                .await
            {
                tracing::warn!("Failed to send ephemeral to member: {}", e);
            } else {
                sent_count += 1;
            }
        }

        tracing::debug!("Sent ephemeral message to group ({} members)", sent_count);
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
    Some(crate::domain::message::SystemMessage::Undelivered {
        sent_at: dm.timestamp,
        preview,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::SystemMessage;
    use crate::proto::content_v1::AnonGroupMessage;

    fn wrap(inner: Content) -> Content {
        Content {
            content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(
                AnonGroupMessage {
                    group_id: vec![0; 16],
                    inner_content: Some(Box::new(inner)),
                },
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
    }
}

/// Production reaction fanout adapter, with injected mailbox effects for tests.
/// A partial send retains the same action locally: some peers have observed it.
/// Zero confirmed deliveries fails, including a group with no ready recipients.
async fn dispatch_reaction_content<R, G, S, F>(
    group_id: &AnonGroupId,
    inner_content: Content,
    gate: G,
    mut send: S,
) -> Result<(), AnonGroupError>
where
    G: FnOnce() -> Result<Vec<R>, AnonGroupError>,
    S: FnMut(R, Content) -> F,
    F: std::future::Future<Output = Result<(), AnonGroupError>>,
{
    let recipients = gate()?;
    if recipients.is_empty() {
        return Err(AnonGroupError::MessageError(
            "Reaction group has no ready remote recipients".into(),
        ));
    }
    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(
            content_v1::AnonGroupMessage {
                group_id: group_id.to_vec(),
                inner_content: Some(Box::new(inner_content)),
            },
        ))),
        ephemeral_reply_to: None,
        timestamp: TimestampNanos::now().as_u64(),
        padding: AnonGroupService::generate_padding(160),
    };
    let mut delivered = 0;
    let mut failed = 0;
    let mut first_error = None;
    for recipient in recipients {
        match send(recipient, content.clone()).await {
            Ok(()) => delivered += 1,
            Err(error) => {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    if delivered == 0 {
        return Err(first_error
            .unwrap_or_else(|| AnonGroupError::MessageError("Reaction delivery failed".into())));
    }
    if failed > 0 {
        tracing::warn!(%group_id, delivered, failed, "Reaction partially delivered; retaining original action without automatic retry");
    }
    Ok(())
}

#[cfg(test)]
#[path = "../anonymous_reaction_test_fixture.rs"]
mod reaction_test_fixture;

#[cfg(test)]
mod reaction_outcome_tests {
    use super::*;
    use crate::domain::UserId;
    use crate::domain::message::ConversationType;
    use crate::services::conversation::ConversationError;
    use prost::Message as _;
    use reaction_test_fixture::{Fixture, assert_encoded_action};
    use std::cell::RefCell;

    #[tokio::test]
    async fn actual_group_reaction_adapter_gates_total_failure_and_keeps_partial_action() {
        for remove in [false, true] {
            for mode in [
                "blocked",
                "missing",
                "unready",
                "all_failed",
                "partial",
                "delivered",
            ] {
                let fixture = Fixture::new(ConversationType::AnonGroup, remove);
                let group = AnonGroupId::new();
                let recipients = vec![
                    (UserId::new(), [1; 32]),
                    (UserId::new(), [2; 32]),
                    (UserId::new(), [3; 32]),
                ];
                let captured = RefCell::new(Vec::new());
                let inner =
                    crate::services::gateway::builders::build_reaction_content_with_channel_at(
                        "A",
                        remove,
                        &fixture.author.to_string(),
                        100,
                        None,
                        20,
                    );
                let dispatch = dispatch_reaction_content(
                    &group,
                    inner,
                    || match mode {
                        "blocked" => Err(AnonGroupError::GroupBlocked(group)),
                        "missing" => Err(AnonGroupError::GroupNotFound(group)),
                        "unready" => Ok(vec![]),
                        _ => Ok(recipients),
                    },
                    |(_, key), content| {
                        let encoded = content.encode_to_vec();
                        let decoded = Content::decode(encoded.as_slice()).unwrap();
                        let Some(content_v1::content::Content::AnonGroupMessage(wrapped)) =
                            decoded.content
                        else {
                            panic!("group wrapper");
                        };
                        assert_eq!(wrapped.group_id, group.to_vec());
                        assert_encoded_action(
                            wrapped.inner_content.as_deref().unwrap(),
                            remove,
                            20,
                            &fixture.author,
                        );
                        captured.borrow_mut().push(encoded);
                        async move {
                            tokio::task::yield_now().await;
                            if mode == "all_failed" || (mode == "partial" && key != [2; 32]) {
                                Err(AnonGroupError::MessageError(
                                    "injected mailbox failure".into(),
                                ))
                            } else {
                                Ok(())
                            }
                        }
                    },
                );
                fixture
                    .finish(
                        async { dispatch.await.map_err(ConversationError::from) },
                        remove,
                        20,
                        matches!(mode, "partial" | "delivered"),
                    )
                    .await;
                let captured = captured.borrow();
                assert_eq!(
                    captured.len(),
                    if matches!(mode, "all_failed" | "partial" | "delivered") {
                        3
                    } else {
                        0
                    }
                );
                // Same envelope/action for each attempted peer, no repair/re-toggle.
                assert!(captured.windows(2).all(|pair| pair[0] == pair[1]));
            }
        }
    }
}
