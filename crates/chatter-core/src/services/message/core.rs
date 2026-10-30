//! Core message operations.
//!
//! Send, edit, delete, reply, forward, and ephemeral message operations.

use std::path::Path;

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId};
use crate::domain::message::{Message, MessageContent};
use crate::mailbox::group::GroupFanout;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::attachment::AttachmentService;

use super::{MessageError, MessageService};

/// A send that may or may not have landed and is not resent on its own: the
/// user's retry decides. Carried by `MessageError::DmError`.
pub(crate) const DELIVERY_UNCONFIRMED: &str = "Delivery unconfirmed";
pub(crate) const SHIELD_WAITING_FOR_TOKENS: &str =
    "Shield is getting ready — will send automatically";
pub(crate) const BOOTSTRAP_TIMED_OUT: &str = "Secure channel setup timed out. Tap Retry";

impl MessageError {
    /// User-facing label for a send that did not land.
    ///
    /// `None` is a transient failure: the bubble reads "not sent · retry".
    /// `Some` is the short reason shown after "not sent · ". Size refusals
    /// cannot be retried unchanged. Unknown failures are not guessed to be
    /// transient.
    pub fn permanent_send_failure_reason(&self) -> Option<&'static str> {
        use crate::error::NetworkError;
        match self {
            Self::Network(NetworkError::RequestNotSent(_)) => Some("Message not submitted"),
            Self::Network(e) if e.is_retryable() || e.is_transport_error() => None,
            Self::Network(NetworkError::MismatchedDevices { .. }) => None,
            // SpiceDB down answers PermissionDenied. That is a delivery miss,
            // not a rule the user broke.
            Self::Network(NetworkError::PermissionDenied) => Some("Not delivered"),
            Self::ShieldNotReady => Some(SHIELD_WAITING_FOR_TOKENS),
            Self::NotAuthor => Some("Not allowed"),
            Self::Network(NetworkError::InvalidRequest(reason))
                if reason.contains("too large") => Some("Message too long"),
            Self::InvalidContent(reason) if reason.contains("too large") => {
                Some("Message too long")
            }
            Self::Network(NetworkError::InvalidRequest(_))
            | Self::InvalidContent(_)
            | Self::InvalidMessage(_) => Some("Message rejected"),
            Self::NotLoggedIn | Self::Network(NetworkError::Unauthenticated) => {
                Some("Sign in required")
            }
            Self::DmError(reason) if reason.as_str() == DELIVERY_UNCONFIRMED => {
                Some(DELIVERY_UNCONFIRMED)
            }
            Self::RecipientIdentityChanged { .. } => Some("Identity changed"),
            Self::RecipientKeyUnverified { .. } => Some("Key not verified"),
            Self::MembershipNotVerified { .. } => Some("Membership not verified"),
            Self::ChannelNotFound(_) | Self::Network(NetworkError::NotFound(_)) => {
                Some("Conversation unavailable")
            }
            _ => Some("Cannot send message"),
        }
    }
}

fn update_retry_status(
    db: &crate::storage::database::Database,
    id: &MessageId,
    status: &str,
    reason: Option<&str>,
) -> Result<(), crate::error::StorageError> {
    let conn = db.connection()?;
    conn.execute("UPDATE messages SET delivery_status=CASE WHEN delivery_status IN ('sent','delivered','read') THEN delivery_status ELSE ?2 END, failure_reason=CASE WHEN delivery_status IN ('sent','delivered','read') OR ?2='sent' THEN NULL ELSE ?3 END WHERE id=?1", rusqlite::params![id.to_string(), status, reason])?;
    Ok(())
}

impl MessageService {
    /// Change retry progress without overwriting a concurrently arriving receipt.
    pub(crate) fn finish_retry_row(
        &self,
        id: &MessageId,
        status: &str,
        reason: Option<&str>,
    ) -> Result<Message, MessageError> {
        update_retry_status(&self.database, id, status, reason)?;
        self.message_repo
            .get(id)?
            .ok_or(MessageError::MessageNotFound(*id))
    }

    /// Send a message to a channel.
    ///
    /// Encrypts with sender keys if channel requires encryption. Channel
    /// sends go to the server channel only: `group_fanout` is accepted but
    /// not used here (a second pairwise copy double-stored the message, see
    /// below); pairwise group-mailbox fan-out is the anon-group path.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        attachment_pointers: Vec<AttachmentPointer>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
        quote: Option<&crate::services::gateway::QuoteIntent>,
        sent_at: u64,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Server-visible content is the text-only placeholder — the server
        // never sees plaintext on encrypted channels anyway, and unencrypted
        // channels only ever ship text.
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(
                trimmed_text.to_string(),
            )),
        };

        let require_encryption = self
            .channel_requires_encryption(channel_id, channel_client)
            .await?;

        // Sender-key channel: every key decision reads the VERIFIED membership.
        // No verified state, a failed verification or a fork blocks the send
        // before anything is encrypted; otherwise the state stamps the
        // DataMessage for the receivers' fork / staleness cross-check.
        let group_state = if require_encryption {
            self.sync_channel_state_if_behind(channel_id);
            Some(self.group_gate().for_distribution(channel_id)?)
        } else {
            None
        };

        // Disappearing messages: READ this conversation's configured timer (the
        // channel's `default_ttl_seconds`, written by `set_disappearing_timer`)
        // and ARM the message with it. This is the load-bearing wire that was
        // cut: every send used to hardcode `None`. Signal semantics — the timer
        // starts at SEND time: the server schedules a Temporal delete workflow
        // from receipt (see `scheduleEphemeralMessageCleanup`), and the E2E
        // marker set on the DataMessage below lets recipients expire their own
        // local copy. `0`/absent = disabled.
        let disappearing_ttl_secs: Option<u32> = self
            .channel_repo
            .get(channel_id)
            .ok()
            .flatten()
            .and_then(|c| c.config.default_ttl_seconds)
            .filter(|s| *s > 0);

        // E2E plaintext that rides inside the sender-key ciphertext: use the
        // pairwise `content_v1::DataMessage` proto so attachment pointers
        // (CDN key + per-blob AES key) travel with the message and receivers
        // can actually download + decrypt the blob.
        // The devices this send is for. Recorded before encryption so a later
        // failure still has an original recipient set, and a resend cannot
        // grow it. Receipts use the same rows.
        self.snapshot_channel_receipt_targets(*channel_id, sent_at, auth_client, channel_client)
            .await?;

        let plaintext_bytes = {
            use prost::Message as _;
            let mut dm = crate::proto::content_v1::DataMessage {
                body: trimmed_text.to_string(),
                attachments: attachment_pointers.clone(),
                channel_id: channel_id.to_string(),
                timestamp: sent_at,
                mentioned_integrations: mentioned_integrations
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
                targets_ai_bot,
                // Carry the reply quote on the wire so the RECEIVER's reply
                // bubble renders (channel replies used to drop this — the quote
                // rode only the sender's local echo, so recipients saw plain
                // text). Mirrors the DM path (build_reply_content).
                quote: self.build_wire_quote(quote),
                // Thread root on the wire. The server-side `parent_id` below is
                // only seen by clients that fetch through GetMessages; mailbox
                // recipients decode this DataMessage, so without this field a
                // thread reply lands in their main channel.
                thread_id: thread_id.map(|id| id.to_string()).unwrap_or_default(),
                // Arm the E2E disappearing marker so recipients (and our own
                // linked devices) can expire their local copy on their own clock.
                ephemeral: disappearing_ttl_secs.is_some(),
                ephemeral_ttl_seconds: disappearing_ttl_secs.unwrap_or(0),
                ..Default::default()
            };
            if let Some(state) = &group_state {
                super::group::stamp_group_data_message(&mut dm, state);
                dm.profile_key = self.group_profile_key(state);
            }
            let wrap = crate::proto::content_v1::Content {
                timestamp: dm.timestamp,
                content: Some(crate::proto::content_v1::content::Content::DataMessage(dm)),
                ..Default::default()
            };
            wrap.encode_to_vec()
        };

        // Sender-key channel: members who lack our current key get it BEFORE
        // this message is encrypted, or they can never read it.
        if require_encryption {
            self.distribute_sender_key_to_new_members(channel_id)
                .await?;
        }

        let (content_to_send, skdm_opt) = self
            .prepare_encrypted_content(
                channel_id,
                &plaintext_bytes,
                content,
                require_encryption,
                sent_at,
            )
            .await?;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm)
                .await?;
        }

        // Call server
        tracing::debug!(
            channel_id = %channel_id,
            text_len = trimmed_text.len(),
            "[SEND_MESSAGE] Sending message to server API"
        );
        // Arm the server-side ephemeral pipeline: a non-None TTL makes the
        // message handler schedule a Temporal delete workflow for it.
        let (ephemeral, ephemeral_ttl) = match disappearing_ttl_secs {
            Some(secs) => (
                true,
                Some(prost_types::Duration {
                    seconds: i64::from(secs),
                    nanos: 0,
                }),
            ),
            None => (false, None),
        };
        crate::services::gateway::submission::mark_submission_started()?;
        let request = chat_v1::SendMessageRequest {
            channel_id: channel_id.to_string(),
            content: Some(content_to_send.clone()),
            parent_id: thread_id.map(ToString::to_string).unwrap_or_default(),
            ephemeral,
            ephemeral_ttl,
            client_request_id: uuid::Uuid::new_v4().to_string(),
            mentioned_integrations: mentioned_integrations
                .iter()
                .map(ToString::to_string)
                .collect(),
            targets_ai_bot,
            mentioned_file_ids: attachment_ids.iter().map(ToString::to_string).collect(),
        };
        let request = super::channel_send::retain_channel_request(
            request,
            group_state.as_ref().map(|state| state.state_hash.to_vec()),
        )?;
        let proto_message = message_client.send_prepared_message(request).await?;
        super::channel_send::retain_channel_response(&proto_message)?;
        crate::services::gateway::submission::bind_original_row(proto_message.id.clone())?;
        tracing::info!(
            channel_id = %channel_id,
            message_id = %proto_message.id,
            "[SEND_MESSAGE] Server API success, message_id={}", proto_message.id
        );

        // Server-managed channels: delivery happens via server → device inbox fanout
        // only. Pairwise group-mailbox fanout is reserved for anon groups (which go
        // through AnonGroupService::send_message). Sending both here causes receivers
        // to store the same message twice — once with plaintext (after SKDM arrives),
        // once as an "[Encrypted message]" placeholder (race where pairwise copy
        // arrives before the sender key).
        let _ = group_fanout;
        let _ = attachment_pointers;

        // Convert to domain message and preserve attachment IDs.
        // Derive ConversationType from the channel row so identified
        // channels don't get mistagged as AnonGroup.
        let conv_type =
            super::conversion::conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut message = super::conversion::proto_to_outgoing_channel_message(
            &proto_message,
            sender_id,
            conv_type,
            sent_at,
            require_encryption,
        )?;
        message.attachments = attachment_ids;
        if !require_encryption && !super::channel_send::has_channel_queue() {
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.database.clone(),
            )
            .bind_send_timestamp(
                *channel_id,
                message.conversation_type.clone(),
                sender_id,
                sent_at,
                message.sent_at,
            )?;
        }

        // IMPORTANT: Check if server returned a different channel_id
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_MESSAGE] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            // Fix: Use the input channel_id, not the server's
            message.conversation_id = *channel_id;
        }

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // The durable queue checkpoints the accepted response, then replaces
        // its parked bubble without colliding with author/timestamp dedupe.
        if super::channel_send::has_channel_queue() {
            return Ok(message);
        }

        // Store locally
        tracing::info!(
            channel_id = %message.conversation_id,
            message_id = %message.id,
            text_len = trimmed_text.len(),
            "[SEND_MESSAGE] Storing message locally"
        );
        self.message_repo.insert_or_ignore(&message)?;
        if let Some(stored) = self.message_repo.get(&message.id)? {
            message.delivery_status = stored.delivery_status;
        }
        self.sync_channel_receipt_targets(*channel_id, message.sent_at, auth_client)
            .await;
        tracing::info!(
            channel_id = %channel_id,
            message_id = %message.id,
            "[SEND_MESSAGE] Message stored successfully"
        );

        // Disappearing messages — SENDER local half. If this send armed a
        // disappearing timer (V29 read the channel's `default_ttl_seconds`),
        // track OUR OWN copy so the periodic reaper removes it once the timer
        // elapses — otherwise the message vanishes from the server + recipient
        // but lingers forever on the sender's device. Timer starts at send
        // time (send-time-start semantics, matching V29 / the server's
        // Temporal-from-receipt schedule). Best-effort; never fail the send.
        if let Some(secs) = disappearing_ttl_secs {
            if let Err(e) = self.ephemeral_service.track_armed(message.id, secs) {
                tracing::warn!(
                    message_id = %message.id,
                    error = %e,
                    "failed to arm local disappearing timer for sent message"
                );
            }
        }

        Ok(message)
    }

    /// Send a message to a channel (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    ///
    /// # Arguments
    /// * `attachment_ids` - IDs for server API and local storage
    /// * `attachment_pointers` - Full AttachmentPointer protos for E2E encrypted group delivery
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        attachment_pointers: Vec<AttachmentPointer>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
        quote: Option<&crate::services::gateway::QuoteIntent>,
    ) -> Result<Message, MessageError> {
        // Text-only real channels have a durable predeposit queue. Other
        // transports and attachment preparation retain their existing owner.
        if attachment_ids.is_empty()
            && attachment_pointers.is_empty()
            && self.channel_repo.get(channel_id)?.is_some_and(|channel| {
                matches!(
                    channel.channel_type,
                    crate::domain::channel::ChannelType::Public
                        | crate::domain::channel::ChannelType::Private
                )
            })
        {
            return self
                .send_durable_channel_text(
                    channel_id,
                    text,
                    thread_id,
                    mentioned_integrations,
                    targets_ai_bot,
                    quote,
                )
                .await;
        }
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        // One clock for the receipt snapshot inside `send_message` and, if
        // that send fails, the stored bubble. A retry looks recipients up by it.
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.send_message(
                &mut message_client,
                &mut auth_client,
                &mut channel_client,
                group_fanout,
                channel_id,
                text,
                attachment_ids.clone(),
                attachment_pointers,
                thread_id,
                mentioned_integrations.clone(),
                targets_ai_bot,
                quote,
                sent_at,
            ),
        )
        .await
        .unwrap_or_else(|_| Err(MessageError::Network(crate::error::NetworkError::Timeout)));
        match result {
            Ok(message) => Ok(message),
            Err(error) => {
                let mut message = Message::new_text(
                    *channel_id,
                    super::conversion::conversation_type_for_channel(
                        &self.channel_repo,
                        channel_id,
                    ),
                    Some(self.settings_repo.current_user_id()?),
                    text.to_string(),
                    sent_at,
                );
                message.attachments = attachment_ids;
                message.thread_id = thread_id.copied();
                message.mentioned_integrations = mentioned_integrations;
                message.targets_ai_bot = targets_ai_bot;
                message.delivery_status = "failed".into();
                message.failure_reason = error.permanent_send_failure_reason().map(str::to_owned);
                self.message_repo.insert_outgoing(&message)?;
                tracing::warn!(error = %error, message_id = %message.id, "Channel send stored as failed");
                Ok(message)
            }
        }
    }

    /// Retry a stored channel send.
    ///
    /// A public channel (and Saved Messages) is a new send: the post is
    /// world-readable, or it has no other recipient. A private channel is not.
    /// Posting it again would hand it to whoever is a member now, including
    /// someone who joined after the original attempt, and a new sender-key
    /// ciphertext is readable by anyone holding the current key. That retry
    /// is pairwise, and only to devices recorded for this `sent_at` that are
    /// still members. A stored failure label does not block either path.
    pub(crate) async fn retry_channel_message(
        &self,
        id: &MessageId,
    ) -> Result<Message, MessageError> {
        let mut failed = self
            .message_repo
            .get(id)?
            .ok_or(MessageError::MessageNotFound(*id))?;
        if failed.conversation_type != crate::domain::message::ConversationType::Channel
            || failed.sender_id != Some(self.settings_repo.current_user_id()?)
            || failed.delivery_status != "failed"
            || failed.is_deleted
        {
            return Err(MessageError::InvalidContent(
                "message cannot be retried".into(),
            ));
        }
        if let Some(queued) = self.durable_channel_retry(&failed)? {
            return Ok(queued);
        }
        let pointers = self.pointers_for_resend(&failed.attachments)?;
        let quote = match (failed.reply_to_author_id, failed.reply_to) {
            (Some(author), Some(parent)) => self.message_repo.get(&parent)?.map(|parent| {
                crate::services::gateway::QuoteIntent {
                    target_author_id: author,
                    target_timestamp: parent.sent_at,
                    target_preview: failed.reply_to_text.clone(),
                }
            }),
            _ => None,
        };
        let channel = self.channel_repo.get(&failed.conversation_id)?;
        let fresh_send = channel.as_ref().is_some_and(|c| {
            matches!(
                c.channel_type,
                crate::domain::channel::ChannelType::Public
                    | crate::domain::channel::ChannelType::SavedMessages
            )
        });
        if !fresh_send {
            return self.retry_private_channel_message(&mut failed).await;
        }
        // New post, new clock. The failed row is replaced when the server
        // assigns a different id.
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        // `send_message` marks its actual delivery boundary after preparation.
        let result = self
            .send_message(
                &mut self.message_client(),
                &mut self.auth_client(),
                &mut self.channel_client(),
                None,
                &failed.conversation_id,
                failed.as_text().unwrap_or(""),
                failed.attachments.clone(),
                pointers,
                failed.thread_id.as_ref(),
                failed.mentioned_integrations.clone(),
                failed.targets_ai_bot,
                quote.as_ref(),
                sent_at,
            )
            .await;
        match result {
            Ok(mut sent) => {
                if let Some(quote) = quote {
                    sent = self
                        .persist_outgoing_reply_context(sent, &quote)
                        .map_err(|(_, e)| e)?;
                }
                if sent.id != *id {
                    self.message_repo.delete(id)?;
                }
                Ok(sent)
            }
            Err(error) => {
                // Preserve old-row receipts, but they do not confirm this fresh
                // public post. Its durable claim stays unresolved on error.
                self.finish_retry_row(&failed.id, "failed", error.permanent_send_failure_reason())?;
                Err(error)
            }
        }
    }

    /// Pairwise retry of one private channel message. Does not re-snapshot:
    /// a second snapshot would add devices the original send never had.
    async fn retry_private_channel_message(
        &self,
        failed: &mut Message,
    ) -> Result<Message, MessageError> {
        match self.resend_stored_channel_message(failed).await {
            Ok(_) => self.finish_retry_row(&failed.id, "sent", None),
            Err(error) => {
                let current = self.finish_retry_row(
                    &failed.id,
                    "failed",
                    error.permanent_send_failure_reason(),
                )?;
                if matches!(
                    current.delivery_status.as_str(),
                    "sent" | "delivered" | "read"
                ) {
                    return Ok(current);
                }
                Err(error)
            }
        }
    }

    /// Pointers for a resend: every attachment must already be uploaded
    /// (`Attachment::to_pointer`, which keeps the voice-note flag), so its
    /// ciphertext is never uploaded again.
    pub(crate) fn pointers_for_resend(
        &self,
        attachment_ids: &[AttachmentId],
    ) -> Result<Vec<crate::proto::content_v1::AttachmentPointer>, MessageError> {
        attachment_ids
            .iter()
            .map(|id| {
                self.attachment_repo
                    .get(id)?
                    .and_then(|attachment| attachment.to_pointer())
                    .ok_or_else(|| {
                        MessageError::InvalidContent(format!(
                            "attachment {id} can no longer be sent"
                        ))
                    })
            })
            .collect()
    }

    /// Send a message with file attachments (high-level API).
    ///
    /// This method handles the complete flow:
    /// 1. Upload each file to CDN with encryption
    /// 2. Build AttachmentPointer protos
    /// 3. Send message via server API
    /// 4. Send via group mailboxes (if encrypted channel)
    ///
    /// # Arguments
    /// * `group_fanout` - Group member mailbox info for E2E encrypted delivery
    /// * `channel_id` - Target channel
    /// * `text` - Message text
    /// * `file_paths` - Local file paths to upload as attachments
    /// * `thread_id` - Optional thread root for thread replies
    /// * `mentioned_integrations` - IDs of mentioned integrations (bots)
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_files(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        file_paths: Vec<&Path>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
        quote: Option<&crate::services::gateway::QuoteIntent>,
    ) -> Result<Message, MessageError> {
        // Upload attachments with encryption
        let attachment_service =
            AttachmentService::new(self.attachment_repo.clone(), self.grpc_pool.clone());
        let mut attachment_ids = Vec::with_capacity(file_paths.len());
        let mut attachment_pointers = Vec::with_capacity(file_paths.len());

        for file_path in &file_paths {
            tracing::info!(
                path = %file_path.display(),
                "Encrypting and uploading attachment to CDN"
            );

            match attachment_service.upload_to_cdn_encrypted(file_path).await {
                Ok(result) => {
                    tracing::info!(
                        cdn_key = %result.cdn_key,
                        attachment_id = %result.attachment_id,
                        filename = %result.file_name,
                        "Attachment uploaded to CDN"
                    );
                    attachment_ids.push(result.attachment_id);
                    attachment_pointers.push(result.to_attachment_pointer());
                }
                Err(e) => {
                    tracing::error!(
                        path = %file_path.display(),
                        error = %e,
                        "Failed to upload attachment to CDN"
                    );
                    return Err(MessageError::AttachmentError(e.to_string()));
                }
            }
        }

        // Send message with uploaded attachments
        self.send(
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
            quote,
        )
        .await
    }

    /// Send a message with pre-uploaded attachment pointers.
    ///
    /// Like `send_with_files` but skips the upload step.
    pub async fn send_with_pointers(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
        quote: Option<&crate::services::gateway::QuoteIntent>,
    ) -> Result<Message, MessageError> {
        self.send(
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
            quote,
        )
        .await
    }

    /// Send control content (reactions, typing, pin, edit, delete, …) to
    /// every channel member's device inbox as a pairwise sealed-sender
    /// envelope.
    ///
    /// Replaces the former group-mailbox transport: the server-managed
    /// channels already expose membership, so the additional mailbox buys
    /// no privacy and costs a second polling loop.
    pub async fn send_control_content(
        &self,
        _message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        _channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        _group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        let _ = self.settings_repo.current_user_id()?;

        self.fanout_control_to_device_inboxes(auth_client, channel_id, &content)
            .await
    }

    /// Send control content to a channel (simplified API — no client params).
    pub async fn send_control(
        &self,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_control_content(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            content,
            group_fanout,
        )
        .await
    }

    /// Encrypt pre-serialized control content per (member, device) and ship
    /// each ciphertext through the device inbox, UAK-authorized like a 1:1 DM.
    /// Channel members already know each other, so this spends no anonymous
    /// mailbox token (OPEN-ITEMS §120).
    ///
    /// Skips our own current device (we already applied the change locally)
    /// but fans to our own linked devices for Signal-style multi-device sync.
    ///
    /// Every deposit is logged in the send log, so a member device that
    /// cannot decrypt its copy gets these exact bytes again on its
    /// `DecryptionErrorMessage` (`dm_resend.rs`, OPEN-ITEMS §5).
    async fn fanout_control_to_device_inboxes(
        &self,
        auth_client: &mut AuthClient,
        channel_id: &ChannelId,
        content: &crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        use crate::storage::repositories::settings_repo::keys;
        use prost::Message as _;

        let our_user_id = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Fan out to the CURRENT server member list, not the local cache. The
        // cache (`memberships`) has an FK to `channels`, so a member who never
        // materialised a local channel row has an empty cache — which silently
        // dropped reactions/pins/edits/typing before. The server list is the
        // source of truth; fall back to the cache only if the RPC failed.
        let mut members: Vec<crate::domain::UserId> =
            self.refresh_channel_members(channel_id).await;
        if members.is_empty() {
            members = self
                .channel_repo
                .list_members(channel_id)
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.user_id)
                .collect();
        }
        if members.is_empty() {
            return Ok(());
        }

        let plaintext = content.encode_to_vec();
        let mut message_client = self.message_client();
        let mut success = 0usize;
        let mut errors = 0u32;
        let mut deposits = Vec::new();

        for member_uid in &members {
            let member_id = member_uid.to_string();
            let is_self = member_id == our_user_id;

            let outcome = self
                .deposit_sealed_to_all_devices(
                    auth_client,
                    &mut message_client,
                    &member_id,
                    &plaintext,
                    Some(channel_id.as_bytes()),
                )
                .await;
            deposits.extend(
                outcome
                    .deposits
                    .iter()
                    .map(|&(device, server_ts)| (*member_uid, device, server_ts)),
            );
            success += outcome.delivered;
            if outcome.delivered == 0 && !is_self {
                errors += 1;
            }
        }

        self.record_pairwise_send(content, &deposits, Some(channel_id.as_bytes()));

        tracing::debug!(
            channel_id = %channel_id,
            success,
            errors,
            "Control content fanout via device inbox complete"
        );
        Ok(())
    }

    /// Pull the current member list from the server and upsert it into the
    /// local `channel_memberships` table. Fire-and-forget: a failure to
    /// refresh doesn't block the outgoing control send — the fanout simply
    /// falls back to whatever the cache holds.
    /// Pull the current member list from the server. Returns the member ids so
    /// the caller can fan out to them directly — the local `memberships` cache
    /// is unreliable for this (its FK to `channels` means the best-effort upsert
    /// below silently drops when the caller has no local channel row, e.g. a
    /// member who never opened the channel — which previously made control
    /// content like reactions/pins/edits vanish, since fanout keyed off the
    /// empty cache). The server list is the source of truth for recipients.
    async fn refresh_channel_members(&self, channel_id: &ChannelId) -> Vec<crate::domain::UserId> {
        let mut channel_client = self.grpc_pool.channel_client();
        // Full membership, paginated: this list is the fan-out recipient set,
        // so a single-page fetch meant members past the server's page size
        // received no channel content at all (OPEN-ITEMS §16).
        let members = match channel_client
            .get_all_channel_members(&channel_id.to_string())
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %e,
                    "Skipping member refresh (RPC failed)"
                );
                return Vec::new();
            }
        };

        let memberships = match members
            .iter()
            .map(|proto| {
                crate::services::channel::converters::proto_to_domain_membership(channel_id, proto)
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(memberships) => memberships,
            Err(e) => {
                tracing::debug!(channel_id = %channel_id, error = %e, "Member refresh skipped (roster did not convert)");
                return Vec::new();
            }
        };
        // Full list: drop departed members. A missing local channel row fails
        // the insert (FK) and rolls the replace back; fan-out still uses the
        // server ids below.
        if let Err(e) = self
            .channel_repo
            .replace_memberships(channel_id, &memberships)
        {
            tracing::debug!(channel_id = %channel_id, error = %e, "Member refresh did not update local roster");
        }
        memberships.into_iter().map(|m| m.user_id).collect()
    }

    /// Delete a message from a channel.
    pub async fn delete_message(
        &self,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<(), MessageError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Get existing message to verify ownership
        let existing = self
            .message_repo
            .get(message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*message_id))?;

        // Verify sender matches current user (only if sender_id is known)
        if let Some(sender_id) = existing.sender_id {
            if sender_id != current_user {
                return Err(MessageError::NotAuthor);
            }
        }

        // Call server: SpiceDB authorizes us as the message's author.
        let mut message_client = self.message_client();
        message_client
            .delete_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Soft delete locally - keep for history/sync
        let mut message = existing;
        message.mark_deleted();
        self.message_repo.update(&message)?;

        Ok(())
    }

    /// Authorize and prepare the entire local selection before one server RPC.
    /// The local soft-delete is one SQLite statement after server commit.
    pub async fn delete_messages(
        &self,
        channel_id: &ChannelId,
        message_ids: &[MessageId],
    ) -> Result<(), MessageError> {
        if message_ids.is_empty() {
            return Ok(());
        }
        let current_user = self.settings_repo.current_user_id()?;
        let mut seen = std::collections::HashSet::new();
        let mut targets = Vec::with_capacity(message_ids.len());
        for id in message_ids {
            if !seen.insert(*id) {
                continue;
            }
            let existing = self
                .message_repo
                .get(id)?
                .ok_or_else(|| MessageError::MessageNotFound(*id))?;
            if existing.conversation_id != *channel_id {
                return Err(MessageError::NotAuthor);
            }
            if existing
                .sender_id
                .is_some_and(|sender| sender != current_user)
            {
                return Err(MessageError::NotAuthor);
            }
            targets.push((id.to_string(), channel_id.to_string()));
        }
        self.message_client().delete_messages(targets).await?;
        self.message_repo.soft_delete_batch(message_ids)?;
        Ok(())
    }

    /// Build the Signal-style wire `Quote` for an outgoing reply so the
    /// RECEIVER's reply bubble renders. Author + id come straight from the
    /// intent (canonical, works even if the parent isn't local); preview text
    /// and the thumbnail are best-effort from the locally-resolved parent.
    /// Returns `None` when there is no quote.
    pub(super) fn build_wire_quote(
        &self,
        quote: Option<&crate::services::gateway::QuoteIntent>,
    ) -> Option<crate::proto::content_v1::Quote> {
        let quote = quote?;
        let original = self
            .message_repo
            .get_by_sender_timestamp(&quote.target_author_id, quote.target_timestamp)
            .ok()
            .flatten();
        let atts: Vec<_> = original
            .as_ref()
            .map(|m| {
                m.attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect()
            })
            .unwrap_or_default();
        let preview = quote
            .target_preview
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| {
                original
                    .as_ref()
                    .map(|m| crate::services::gateway::shared::message_reply_preview(m, &atts))
            })
            .unwrap_or_default();
        Some(
            crate::services::gateway::shared::build_quote_with_attachments(
                &quote.target_author_id.to_string(),
                quote.target_timestamp,
                &preview,
                crate::services::gateway::shared::build_quoted_attachments(&atts),
            ),
        )
    }

    /// Patch an already-stored outgoing message with reply-quote context.
    ///
    /// Used by the outgoing router: channel sends (`send_with_pointers` /
    /// `send_with_files`) do not carry the quote, so the local row is written
    /// without `reply_to`/`reply_to_author_id`/`reply_to_text`. When the user
    /// is replying we fill those columns in here so the quote bubble survives
    /// a channel reload.
    ///
    /// Returns the updated message on success, or `(original, error)` if the
    /// update failed — callers can choose to log and continue.
    pub fn persist_outgoing_reply_context(
        &self,
        mut message: Message,
        quote: &crate::services::gateway::QuoteIntent,
    ) -> Result<Message, (Message, crate::error::StorageError)> {
        // Resolve quoted message locally to fill in any missing fields.
        let original = self
            .message_repo
            .get_by_sender_timestamp(&quote.target_author_id, quote.target_timestamp)
            .ok()
            .flatten();
        self.fill_reply_context(&mut message, quote, original);
        match self.message_repo.update(&message) {
            Ok(()) => Ok(message),
            Err(e) => Err((message, e)),
        }
    }

    /// Set the reply-quote fields on `message` from `quote` and `original`,
    /// the quoted parent as the caller resolved it locally (`None` when it is
    /// not stored: the parent id then derives from the quote timestamp).
    /// Preview text prefers `quote.target_preview`, then the parent's own
    /// preview; the parent's first attachment supplies the thumbnail. Does
    /// not touch storage: `persist_outgoing_reply_context` updates a stored
    /// row, `save_message` fills the row before its insert.
    pub(crate) fn fill_reply_context(
        &self,
        message: &mut Message,
        quote: &crate::services::gateway::QuoteIntent,
        original: Option<Message>,
    ) {
        let original_attachments: Vec<_> = original
            .as_ref()
            .map(|m| {
                m.attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect()
            })
            .unwrap_or_default();
        let reply_to_id = original.as_ref().map(|m| m.id).unwrap_or_else(|| {
            crate::domain::ids::MessageId::from_timestamp(quote.target_timestamp)
        });
        let reply_to_text = quote
            .target_preview
            .clone()
            .or_else(|| {
                original.as_ref().map(|m| {
                    crate::services::gateway::shared::message_reply_preview(
                        m,
                        &original_attachments,
                    )
                })
            })
            .filter(|t| !t.is_empty());
        let quote_atts =
            crate::services::gateway::shared::build_quoted_attachments(&original_attachments);
        let (reply_thumb, reply_thumb_mime) = quote_atts
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));

        message.reply_to = Some(reply_to_id);
        message.reply_to_author_id = Some(quote.target_author_id);
        message.reply_to_text = reply_to_text;
        message.reply_to_thumbnail = reply_thumb;
        message.reply_to_thumbnail_mime = reply_thumb_mime;
    }

    /// Helper to encrypt content based on channel type.
    pub(crate) async fn prepare_encrypted_content(
        &self,
        channel_id: &ChannelId,
        plaintext: &[u8],
        original_content: chat_v1::message::Content,
        require_encryption: bool,
        sent_at: u64,
    ) -> Result<
        (
            chat_v1::message::Content,
            Option<crate::proto::content_v1::SenderKeyDistribution>,
        ),
        MessageError,
    > {
        // Encrypted channel: Use sender keys (group encryption)
        if require_encryption {
            // A locally blocked rotation is definite preparation refusal.
            if self
                .signal_service
                .is_channel_send_blocked(channel_id)
                .map_err(|e| MessageError::EncryptionFailed(e.to_string()))?
            {
                return Err(MessageError::EncryptionFailed(
                    "channel key rotation pending".into(),
                ));
            }
            crate::services::gateway::submission::mark_submission_started()?;
            match self
                .signal_service
                .encrypt_for_group(channel_id, plaintext)
                .await
            {
                Ok((mut group_encrypted, skdm_opt)) => {
                    use prost::Message;
                    // encrypt_for_group stamps a later now(). The envelope
                    // timestamp is outside the sender-key ciphertext, and it
                    // is the clock a recipient names in a DecryptionErrorMessage
                    // when it cannot decrypt. It has to be the same sender
                    // clock as the DataMessage and the local row (`sent_at`).
                    group_encrypted.timestamp = sent_at;
                    let group_bytes = group_encrypted.encode_to_vec();

                    let encrypted_content = chat_v1::message::Content {
                        r#type: Some(chat_v1::message::content::Type::Encrypted(
                            chat_v1::EncryptedData {
                                version: chat_v1::CryptoVersion::SignalProtocol as i32,
                                ciphertext: group_bytes,
                                // Mark as sender key message in associated_data
                                associated_data: b"sender_key".to_vec(),
                            },
                        )),
                    };
                    return Ok((encrypted_content, skdm_opt));
                }
                Err(e) => {
                    // Fail closed: an encryption-required channel must never
                    // downgrade to cleartext.
                    return Err(MessageError::EncryptionFailed(format!(
                        "Group encryption failed for channel {}: {}",
                        channel_id, e
                    )));
                }
            }
        }

        // FAIL CLOSED (F14a): we are about to ship PLAINTEXT on a
        // non-encrypted channel. That is only legitimate for an explicitly
        // PUBLIC channel (world-readable by design). A PRIVATE/group channel
        // must never leave the client in cleartext — if one reached here with
        // require_encryption=false (e.g. a channel row created before this fix,
        // or a server that defaulted it to plaintext), REFUSE rather than
        // silently downgrade. Unknown/not-locally-stored channels are treated
        // as non-public and also refused: a plaintext downgrade must be
        // impossible, not merely unlikely.
        let is_public = self
            .channel_repo
            .get(channel_id)
            .ok()
            .flatten()
            .map(|c| c.channel_type == crate::domain::channel::ChannelType::Public)
            .unwrap_or(false);
        if !is_public {
            return Err(MessageError::EncryptionFailed(format!(
                "refusing to send plaintext on non-public channel {channel_id}: \
                 private/group channels must be end-to-end encrypted \
                 (require_encryption=false). This channel is misconfigured; \
                 recreate it so it is encrypted."
            )));
        }

        // Explicitly PUBLIC channel: plaintext is the intended, visible property.
        Ok((original_content, None))
    }
}

// Re-export proto conversion functions from conversion module
pub use super::conversion::{
    proto_content_to_domain, proto_to_domain_message, proto_to_domain_message_with_sender,
};

#[cfg(test)]
mod send_failure_tests {
    use super::MessageError;
    use crate::error::NetworkError;

    #[test]
    fn only_transient_send_failures_offer_retry() {
        for error in [
            NetworkError::Timeout,
            NetworkError::Unavailable,
            NetworkError::RateLimited,
            NetworkError::ConnectionFailed("offline".into()),
        ] {
            assert_eq!(
                MessageError::Network(error).permanent_send_failure_reason(),
                None
            );
        }
        for error in [
            NetworkError::PermissionDenied,
            NetworkError::InvalidRequest("message too large".into()),
            NetworkError::Unauthenticated,
            NetworkError::Unknown("unclassified".into()),
        ] {
            assert!(
                MessageError::Network(error)
                    .permanent_send_failure_reason()
                    .is_some()
            );
        }
        assert!(
            MessageError::InvalidContent("too large".into())
                .permanent_send_failure_reason()
                .is_some()
        );
        assert_eq!(
            MessageError::Network(NetworkError::PermissionDenied).permanent_send_failure_reason(),
            Some("Not delivered"),
        );
        assert_eq!(
            MessageError::NotAuthor.permanent_send_failure_reason(),
            Some("Not allowed")
        );
    }
}

#[cfg(test)]
mod retry_receipt_tests {
    use super::*;
    #[test]
    fn late_original_receipt_survives_retry_failure_and_bootstrap_completion() {
        use crate::domain::message::ConversationType;
        use crate::storage::repositories::UnifiedMessageRepository;
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(
            crate::storage::database::Database::open(&dir.path().join("receipt.db"), "test-only")
                .unwrap(),
        );
        db.run_migrations()
            .expect("migrate throwaway test database");
        let repo = UnifiedMessageRepository::new(db.clone());
        for kind in [
            ConversationType::Channel,
            ConversationType::ServerDm,
            ConversationType::AnonGroup,
        ] {
            let mut row = Message::new_text(
                ChannelId::new(),
                kind,
                Some(crate::domain::ids::UserId::new()),
                "original".into(),
                7,
            );
            row.delivery_status = "failed".into();
            repo.insert_outgoing(&row).unwrap();
            update_retry_status(&db, &row.id, "sending", None).unwrap();
            // Receipt arrives while the original retry awaits its response.
            db.connection()
                .unwrap()
                .execute(
                    "UPDATE messages SET delivery_status='delivered' WHERE id=?1",
                    rusqlite::params![row.id.to_string()],
                )
                .unwrap();
            update_retry_status(&db, &row.id, "failed", Some("lost response")).unwrap();
            update_retry_status(&db, &row.id, "sending", Some("bootstrap pending")).unwrap();
            update_retry_status(&db, &row.id, "sent", None).unwrap();
            let retained = repo.get(&row.id).unwrap().unwrap();
            assert_eq!(retained.delivery_status, "delivered");
            assert_eq!(retained.failure_reason, None);
            assert_eq!(retained.as_text(), Some("original"));
        }
    }
}
