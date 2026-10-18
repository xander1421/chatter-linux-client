use super::*;

impl MessageService {
    /// Send a DataMessage (text + optional attachments) to a friend over
    /// the channel-free sealed-sender path, and persist it locally as
    /// `ConversationType::ServerDm` keyed by `ChannelId::from(peer_id)`.
    pub async fn send_server_dm(
        &self,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        sent_at: u64,
        quote: Option<&crate::services::gateway::outgoing::QuoteIntent>,
    ) -> Result<DmSendResult, MessageError> {
        // F11: trim is used ONLY to decide emptiness (a whitespace-only body with
        // no attachments is still rejected — F10). A NON-empty body is sent with
        // its ORIGINAL bytes: the wire must carry exactly what the user typed.
        // Whitespace is stripped at DISPLAY time instead (chatter-ui converters/
        // message.rs trims before rendering), never on the wire.
        let trimmed = text.trim();
        if trimmed.is_empty() && attachment_pointers.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        tracing::info!(
            peer_id = %peer_id,
            text_len = text.len(),
            attachment_count = attachment_pointers.len(),
            is_reply = quote.is_some(),
            sent_at,
            "MessageService: send_server_dm called"
        );

        let attachment_ids: Vec<AttachmentId> = attachment_pointers
            .iter()
            .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();

        let (content, reply_ctx, disappearing_ttl_secs) =
            self.server_dm_content(peer_id, text, attachment_pointers, sent_at, quote)?;

        let current_user_id = self.settings_repo.current_user_id()?;
        let conversation_id: ChannelId = (*peer_id).into();
        let mut message = Message::new_text(
            conversation_id,
            ConversationType::ServerDm,
            Some(current_user_id),
            text.to_string(),
            sent_at,
        );
        // Stored as "sending", and bound to the send's admission claim, before
        // anything is encrypted: a crash past here leaves a visible row that
        // the claim and the outbox both name, never a sent message without one.
        message.delivery_status = "sending".into();

        if !attachment_ids.is_empty() {
            message.attachments = attachment_ids.clone();
            // Emptiness decision still uses `trimmed` (a whitespace-only caption
            // is treated as "no caption"), but a real caption is stored with its
            // ORIGINAL bytes to match the wire (F11).
            message.content = if trimmed.is_empty() {
                MessageContent::Mixed(MixedContent::attachments_only(attachment_ids.clone()))
            } else {
                MessageContent::Mixed(MixedContent::new(text.to_string(), attachment_ids.clone()))
            };
        }

        // Persist reply metadata on the local echo so the sender's bubble keeps
        // its quote across reloads (mirrors the wire Quote we just sent).
        if let Some((q, parts)) = reply_ctx {
            message.reply_to = Some(parts.reply_to);
            message.reply_to_author_id = Some(q.target_author_id);
            message.reply_to_text = parts.reply_to_text;
            message.reply_to_thumbnail = parts.reply_to_thumbnail;
            message.reply_to_thumbnail_mime = parts.reply_to_thumbnail_mime;
        }

        self.message_repo.insert_outgoing(&message)?;
        crate::services::gateway::submission::bind_original_row(message.id.to_string())?;
        if let Some(secs) = disappearing_ttl_secs {
            // SENDER local half for sealed DMs. The stamped TTL is kept with
            // its clock stopped; it starts when a send of this row is
            // confirmed, on whichever path (v097 `ephemeral_countdown_on_sent`).
            if let Err(e) = self.ephemeral_service.track_armed(message.id, secs) {
                tracing::warn!(
                    peer_id = %peer_id,
                    error = %e,
                    "failed to arm local disappearing timer for sent ServerDm"
                );
            }
        }

        // Shield, mailbox not established with this friend yet: the content is
        // queued and goes out when the bootstrap completes (`DmBootstrapCompleted`
        // / `MessageSendFailed` settle the row).
        //
        // Normal mode: the sealed outbox owns the row (`dm_outbox`). A send
        // that is not confirmed now stays "sending" and is replayed, exact, on
        // reconnect and after a restart, not handed back as an error.
        //
        // A definite failure is stored as a "failed" bubble the user can retry
        // (`retry_server_dm`), Signal's "Not sent, tap to retry".
        let (bootstrap_initiated, send_error) = if self
            .shield_mode
            .load(std::sync::atomic::Ordering::Acquire)
        {
            match self.send_dm(peer_id, content, &self.mailbox_manager).await {
                Ok(()) => {
                    self.finish_retry_row(&message.id, "sent", None)?;
                    (false, None)
                }
                Err(MessageError::BootstrapInitiated) => (true, None),
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, error = %e, sent_at, "Friend DM not sent; stored as failed");
                    self.finish_retry_row(
                        &message.id,
                        "failed",
                        e.permanent_send_failure_reason(),
                    )?;
                    (false, Some(e))
                }
            }
        } else {
            match self.enqueue_sealed_dm(&message.id, peer_id, &content, None) {
                Ok(generation) => match self
                    .first_sealed_dm_attempt(&message.id, &generation)
                    .await?
                {
                    super::dm_outbox::FirstAttempt::Refused(e) => {
                        tracing::warn!(peer_id = %peer_id, error = %e, sent_at, "Friend DM refused before submission; stored as failed");
                        (false, Some(e))
                    }
                    _ => (false, None),
                },
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, error = %e, sent_at, "Friend DM not queued; stored as failed");
                    self.finish_retry_row(
                        &message.id,
                        "failed",
                        e.permanent_send_failure_reason(),
                    )?;
                    (false, Some(e))
                }
            }
        };

        if let Some(stored) = self.message_repo.get(&message.id)? {
            message.delivery_status = stored.delivery_status;
            message.failure_reason = stored.failure_reason;
        }
        Ok(DmSendResult {
            message: Some(message),
            attachment_ids,
            gif_attachment_ids: vec![],
            sent_at,
            bootstrap_initiated,
            send_error,
        })
    }

    /// The wire `Content` of a friend DM with the given `sent_at` (its
    /// DataMessage timestamp, which is also the bubble's key), plus the reply
    /// parts and disappearing timer the local copy is stored with. Shared by
    /// the first send and by `retry_server_dm`, so a retry carries exactly what
    /// the first attempt would have.
    #[allow(clippy::type_complexity)]
    pub(super) fn server_dm_content<'q>(
        &self,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        sent_at: u64,
        quote: Option<&'q crate::services::gateway::outgoing::QuoteIntent>,
    ) -> Result<
        (
            crate::proto::content_v1::Content,
            Option<(
                &'q crate::services::gateway::outgoing::QuoteIntent,
                super::dm::OutboundReplyParts,
            )>,
            Option<u32>,
        ),
        MessageError,
    > {
        // Resolve the quoted message once via the shared assembler (same path
        // the mailbox DM uses). Carrying the quote on the wire is what lets the
        // receiver's `store_data_message` rebuild the reply bubble.
        let reply_ctx = quote.map(|q| {
            let parts = self.assemble_reply_parts(
                &q.target_author_id,
                q.target_timestamp,
                q.target_preview.as_deref().unwrap_or(""),
            );
            (q, parts)
        });

        // Attachment pointers are compact by construction — the thumbnail is a
        // separate CDN sidecar (thumbnail_cdn_key), not inline, and no field
        // scales with file size. Text and quote metadata can still exceed the
        // device-inbox cap; sealed delivery validates the encoded content before
        // encryption, without applying this transport's limit to Shield.
        let content = match &reply_ctx {
            Some((q, parts)) => crate::services::build_reply_content(
                text,
                attachment_pointers,
                &q.target_author_id.to_string(),
                q.target_timestamp,
                // Core-computed preview (media label / truncated text), not the
                // raw UI preview which is empty for image/gif/video parents.
                parts.reply_to_text.as_deref().unwrap_or(""),
                parts.quote_attachments.clone(),
                sent_at,
            ),
            None => crate::services::build_data_message_content(text, attachment_pointers, sent_at),
        };

        // Signal-style profile-key exchange: piggyback OUR profile key on the
        // DataMessage so the friend can decrypt our encrypted display name +
        // avatar. Auto-enables profile sharing with an accepted friend on first
        // send. Without this the peer only ever sees the "User <id>" fallback.
        let content = self.inject_profile_key_if_sharing(peer_id, content)?;

        // Disappearing messages for SEALED DMs. The server can't schedule a
        // Temporal delete for a sealed message (it can't read the envelope to
        // learn the ttl), so sealed-DM disappearing is entirely local: read
        // this conversation's timer (`conv.disappearing.server_dm.<peer>`,
        // written by `set_disappearing_timer`) and ARM the E2E marker on the
        // DataMessage so the RECIPIENT's `store_data_message` tracks + reaps
        // their local copy — and track OUR OWN copy below. `0`/absent = off.
        // Every message states the timer with its version, so the peer
        // orders it against changes it has already seen.
        let timer = crate::services::disappearing::server_dm_state(&self.settings_repo, peer_id);
        let disappearing_ttl_secs: Option<u32> =
            Some(timer.effective_seconds(sent_at)).filter(|s| *s > 0);
        let mut content = content;
        if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) =
            content.content.as_mut()
        {
            crate::services::disappearing::stamp_timer(dm, timer);
        }

        Ok((content, reply_ctx, disappearing_ttl_secs))
    }

    /// Send a failed friend DM again: same message id, same `sent_at` (so the
    /// recipient sees one message, deduplicated on its timestamp), content
    /// rebuilt from the stored copy. A stored failure label does not block
    /// the retry. Returns the new status, `"sent"` or `"sending"` (queued
    /// behind a Shield bootstrap); on failure the bubble stays `"failed"` and
    /// the error is returned.
    pub(crate) async fn retry_server_dm(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Result<&'static str, MessageError> {
        let message = self
            .message_repo
            .get(message_id)?
            .ok_or(MessageError::MessageNotFound(*message_id))?;
        let me = self.settings_repo.current_user_id()?;
        if message.conversation_type != ConversationType::ServerDm
            || message.sender_id != Some(me)
            || message.delivery_status != "failed"
            || message.is_deleted
        {
            return Err(MessageError::InvalidContent(
                "only a failed friend DM can be retried".into(),
            ));
        }
        let peer_id = UserId::from_uuid(*message.conversation_id.as_uuid());

        let pointers = self.pointers_for_resend(&message.attachments)?;
        let quote = match (message.reply_to_author_id, message.reply_to) {
            (Some(author), Some(parent_id)) => self.message_repo.get(&parent_id)?.map(|parent| {
                crate::services::gateway::outgoing::QuoteIntent {
                    target_author_id: author,
                    target_timestamp: parent.sent_at,
                    target_preview: message.reply_to_text.clone(),
                }
            }),
            _ => None,
        };

        let (mut content, _, _) = self.server_dm_content(
            &peer_id,
            message.as_text().unwrap_or(""),
            pointers,
            message.sent_at,
            quote.as_ref(),
        )?;
        // The TTL stamped when the message was composed, kept (clock stopped)
        // in its `ephemeral_messages` row, not the conversation's timer now:
        // sender and receiver copies must live alike. Version 0 states no
        // conversation timer, as any resend of an old message
        // (`disappearing::stated_timer`).
        let ttl = {
            use rusqlite::OptionalExtension as _;
            self.database
                .read_connection()?
                .query_row(
                    "SELECT ttl_seconds FROM ephemeral_messages WHERE message_id=?1",
                    [message.id.to_string()],
                    |r| r.get::<_, u32>(0),
                )
                .optional()
                .map_err(crate::error::StorageError::from)?
        }
        .filter(|s| *s > 0);
        if let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) =
            content.content.as_mut()
        {
            dm.ephemeral = ttl.is_some();
            dm.ephemeral_ttl_seconds = ttl.unwrap_or(0);
            dm.expire_timer_version = 0;
            dm.disappearing_enabled_until = 0;
            dm.disappearing_activation_seconds = 0;
        }
        if message.failure_reason.as_deref() == Some(super::core::SHIELD_WAITING_FOR_TOKENS) {
            // This row was parked before any submission or device snapshot.
            // It is still a Shield intent, even if the user left Shield while
            // it waited. Never auto-fall through to the direct sealed outbox.
            if !self.shield_mode.load(std::sync::atomic::Ordering::Acquire)
                || self
                    .token_manager
                    .available_token_count()
                    .map_err(|e| MessageError::DmError(e.to_string()))?
                    == 0
            {
                return Err(MessageError::ShieldNotReady);
            }
            // Remove the automatic-retry marker before the first submission.
            // Cancellation/partial delivery is then owned by the send journal,
            // never picked up as a fresh automatic post.
            self.finish_retry_row(&message.id, "failed", None)?;
            let status = match self.send_dm(&peer_id, content, &self.mailbox_manager).await {
                Ok(()) => "sent",
                Err(MessageError::BootstrapInitiated) => "sending",
                Err(error) => {
                    self.finish_retry_row(
                        &message.id,
                        "failed",
                        error.permanent_send_failure_reason(),
                    )?;
                    return Err(error);
                }
            };
            self.finish_retry_row(&message.id, status, None)?;
            return Ok(status);
        }
        // The devices this message was encrypted for, not the peer's devices
        // now. A device linked after the send is not in that set. No fallback
        // when the set is empty: we do not know who the original was for.
        let devices = self.original_dm_devices(peer_id, message.sent_at)?;
        if !self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            // Through the outbox, so an unconfirmed retry is replayed exactly
            // rather than encrypted again.
            let generation =
                self.enqueue_sealed_dm(&message.id, &peer_id, &content, Some(&devices))?;
            self.finish_retry_row(&message.id, "sending", None)?;
            return match self
                .first_sealed_dm_attempt(&message.id, &generation)
                .await?
            {
                super::dm_outbox::FirstAttempt::Sent => Ok("sent"),
                super::dm_outbox::FirstAttempt::Queued => Ok("sending"),
                super::dm_outbox::FirstAttempt::Refused(e) => Err(e),
            };
        }
        let status = match self
            .send_shield_dm_to_devices(&peer_id, content, &devices)
            .await
        {
            Ok(()) => "sent",
            Err(MessageError::BootstrapInitiated) => "sending",
            Err(e) => {
                let current = self.finish_retry_row(
                    &message.id,
                    "failed",
                    e.permanent_send_failure_reason(),
                )?;
                if matches!(
                    current.delivery_status.as_str(),
                    "sent" | "delivered" | "read"
                ) {
                    return Ok("sent");
                }
                return Err(e);
            }
        };
        // `set_friend_dm_delivery_status` writes the status only, so a previous
        // failure label would stay on the bubble. Update the same row.
        self.finish_retry_row(&message.id, status, None)?;
        Ok(status)
    }
}
