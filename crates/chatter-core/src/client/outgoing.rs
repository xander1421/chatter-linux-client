use super::*;

impl ChatterClient {
    // ── High-level messaging ─────────────────────────────────────────

    /// Send a text message (no attachments).
    pub async fn send_text(
        &self,
        conversation: ActiveConversation,
        text: &str,
    ) -> Result<SendResult, SendError> {
        self.send_message(conversation, text, &[], None, None).await
    }

    /// Send a message with optional attachments, quote, and options.
    pub async fn send_message(
        &self,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<SendResult, SendError> {
        self.send_message_attempt(
            uuid::Uuid::new_v4(),
            conversation,
            text,
            pending_attachments,
            quote,
            options,
        )
        .await
        .map_err(|(error, _)| error)
    }

    /// Every send origin funnels through here — the composer, a forward, any
    /// caller of [`send_message`](Self::send_message) — so this is where the
    /// stored row is announced to the chat list ([`DomainEvent::MessageSent`]).
    async fn send_message_attempt(
        &self,
        attempt: uuid::Uuid,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<SendResult, (SendError, Option<String>)> {
        let outcome = self
            .send_message_attempt_inner(
                attempt,
                conversation,
                text,
                pending_attachments,
                quote,
                options,
            )
            .await;
        // A send that stored a row — delivered, queued, or failed and
        // retryable — shows in the conversation, so it moves the row too.
        let stored = match &outcome {
            Ok(result) => Some(result.message.clone()),
            Err((_, row)) => row
                .as_deref()
                .and_then(|id| id.parse().ok())
                .and_then(|id| self.get_message_local(&id))
                .map(Some),
        };
        if let Some(message) = stored {
            self.announce_sent(conversation, message.as_ref(), text.trim());
        }
        outcome
    }

    /// Tell the chat list this device stored an outgoing message: the row's
    /// preview, built from the stored message so an attachment-only send gets
    /// its type label, and its time. `message` is `None` only for a send with
    /// no stored row to read back (queued behind a DM bootstrap).
    pub(super) fn announce_sent(
        &self,
        conversation: ActiveConversation,
        message: Option<&Message>,
        text: &str,
    ) {
        self.services
            .chat_state
            .dismiss_opening_unread(conversation);
        let (preview, at) = match message {
            Some(message) => (self.message_sidebar_preview(message), message.created_at),
            None => (
                messaging::plain_sidebar_preview(&format!("You: {}", text)),
                chrono::Utc::now(),
            ),
        };
        self.services.event_bus.emit_domain(DomainEvent::MessageSent {
            conversation,
            preview,
            at,
        });
    }

    async fn send_message_attempt_inner(
        &self,
        attempt: uuid::Uuid,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<SendResult, (SendError, Option<String>)> {
        let text = text.trim();
        let session = crate::services::gateway::submission::SendSession::capture(self.auth_state());
        let key = self.retain_send_attempt(
            attempt,
            conversation,
            text,
            pending_attachments,
            quote.as_ref(),
            options.as_ref(),
        )
        .map_err(|error| (error, None))?;
        let (outcome, started, original_row) = crate::services::gateway::submission::SEND_SESSION
            .scope(
                session,
                crate::services::gateway::submission::track_submission(
                    self.send_message_untracked(
                        conversation,
                        text,
                        pending_attachments,
                        quote,
                        options,
                    ),
                    Some((
                        self.services.database.clone(),
                        key.clone(),
                        attempt.to_string(),
                    )),
                ),
            )
            .await;
        match outcome {
            Ok(mut result) => {
                // Shield friend DMs return their stored failed row as Ok.
                // A mailbox preconnect failure preceded every deposit byte,
                // even though encryption already started the send journal.
                if matches!(
                    result.send_error.as_ref(),
                    Some(crate::services::message::MessageError::Network(
                        crate::error::NetworkError::RequestNotSent(_)
                    ))
                ) {
                    let error = result.send_error.take().unwrap();
                    send_attempt::settle_failure(
                        &self.services.database,
                        &key,
                        attempt,
                        started,
                        original_row.clone(),
                        SendError::Message(error),
                    )
                    .map_err(|error| (error, original_row.clone()))?;
                    if let Some(id) = original_row.as_deref().and_then(|id| id.parse().ok()) {
                        result.message = self.messages().get_message_local(&id)
                            .map_err(|error| (error.into(), original_row.clone()))?;
                    }
                    return Ok(result);
                }
                // A durable channel queue knows terminal authorization/content
                // refusal separately from transient or uncertain submission.
                if let Some(message) = result.message.as_ref() {
                    if self
                        .messages()
                        .terminal_channel_send(message)
                        .map_err(|_| (SendError::DeliveryUnconfirmed, original_row.clone()))?
                    {
                        self.finish_send_attempt(
                            &key,
                            attempt,
                            Some(message.id.to_string()),
                            true,
                        )
                        .map_err(|error| (error, original_row.clone()))?;
                        return Ok(result);
                    }
                }
                let confirmed = !result.bootstrap_initiated
                    && result.send_error.is_none()
                    && !result.message.as_ref().is_some_and(|m| {
                        matches!(m.delivery_status.as_str(), "failed" | "sending")
                    });
                if !confirmed && !started {
                    // Local preparation failed before the boundary. The stored
                    // failed bubble remains retryable; no remote send exists.
                    self.finish_send_attempt(&key, attempt, None, true)
                        .map_err(|error| (error, original_row.clone()))?;
                    return Ok(result);
                }
                // Normal mode returns a friend DM it could not confirm as a
                // `sending` row with no error: queued in the sealed outbox.
                let outbox_queued = !result.bootstrap_initiated && result.send_error.is_none();
                if !confirmed {
                    if let Some(message) = result.message.as_mut() {
                        *message = if outbox_queued
                            && message.conversation_type
                                == crate::domain::message::ConversationType::ServerDm
                        {
                            // The sealed DM outbox owns this row and settles
                            // it, possibly already: relabelling could revive a
                            // row it just settled `failed` as an orphan
                            // `sending`. Show what it stored.
                            self.messages()
                                .get_message_local(&message.id)
                                .map_err(|error| (error.into(), original_row.clone()))?
                                .unwrap_or_else(|| message.clone())
                        } else {
                            self.messages()
                                .finish_retry_row(
                                    &message.id,
                                    "sending",
                                    Some("Delivery unconfirmed"),
                                )
                                .map_err(|_| (SendError::DeliveryUnconfirmed, original_row.clone()))?
                        };
                    }
                }
                let stored_row = original_row.clone();
                self.finish_send_attempt(
                    &key,
                    attempt,
                    if confirmed {
                        Some(result.message_id_string()).filter(|id| !id.is_empty())
                    } else {
                        original_row.or_else(|| {
                            Some(result.message_id_string()).filter(|id| !id.is_empty())
                        })
                    },
                    confirmed,
                )
                .map_err(|_| (SendError::DeliveryUnconfirmed, stored_row))?;
                Ok(result)
            }
            Err(error) => {
                let error = send_attempt::settle_failure(
                    &self.services.database,
                    &key,
                    attempt,
                    started,
                    original_row.clone(),
                    error,
                )
                .unwrap_or(SendError::DeliveryUnconfirmed);
                Err((error, original_row))
            }
        }
    }

    async fn send_message_untracked(
        &self,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<SendResult, SendError> {
        if text.trim().is_empty() && pending_attachments.is_empty() {
            return Err(SendError::Message(
                crate::services::message::MessageError::InvalidContent(
                    "message must have text or attachments".into(),
                ),
            )
            .before_submission());
        }
        // A secret chat's attachments go over Tor or not at all.
        let uploaded = upload_attachments(
            pending_attachments,
            &self.attachments().for_conversation(&conversation),
            Some(self),
        )
        .await
        .map_err(|e| SendError::from(e).before_submission())?;
        crate::services::gateway::submission::check_send_session()
            .map_err(|_| SendError::NotLoggedIn.before_submission())?;
        // Saved Messages resolves its parent and preview itself, scoped to
        // the Saved conversation (`MessageService::save_message`); the
        // unscoped (sender, sent_at) prefill below could pick a channel or
        // DM row and must not run for it.
        let is_saved = matches!(
            &conversation,
            ActiveConversation::Channel(id) if is_saved_channel(id, self.channels())
        );
        let quote = if let Some(mut q) = quote {
            if q.target_preview.is_none() && !conversation.is_anon() && !is_saved {
                let attachments_svc = self.attachments();
                q.target_preview = self
                    .messages()
                    .get_by_sender_timestamp(&q.target_author_id, q.target_timestamp)
                    .ok()
                    .flatten()
                    .map(|m| {
                        let atts: Vec<_> = m
                            .attachments
                            .iter()
                            .filter_map(|id| attachments_svc.get(id).ok().flatten())
                            .collect();
                        crate::services::gateway::shared::message_reply_preview(&m, &atts)
                    })
                    .filter(|t| !t.is_empty());
            }
            Some(q)
        } else {
            None
        };
        let intent = build_intent(
            conversation,
            text,
            uploaded.pointers,
            quote,
            &options.unwrap_or_default(),
            self.channels(),
        )
        .map_err(SendError::before_submission)?;
        let services = OwnedOutgoingServices::from_client(self);
        route_outgoing_owned(intent, &services).await
    }

    /// Send a message and return a pre-shaped UI result.
    ///
    /// Wraps [`send_message`](Self::send_message) and folds the post-send
    /// shaping (effective message id, delivery status, sidebar preview,
    /// local timestamp, resolved attachments, quoted thumbnail) into a
    /// single struct so the UI crate doesn't have to branch on
    /// `bootstrap_initiated` or N+1 fetch attachments by id.
    pub async fn send_message_ui(
        &self,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<crate::client::messaging::SendMessageUiResult, SendError> {
        self.send_message_ui_attempt(
            uuid::Uuid::new_v4(),
            conversation,
            text,
            pending_attachments,
            quote,
            options,
        )
        .await
    }

    pub(super) async fn send_message_ui_attempt(
        &self,
        attempt: uuid::Uuid,
        conversation: ActiveConversation,
        text: &str,
        pending_attachments: &[PendingAttachment],
        quote: Option<QuoteIntent>,
        options: Option<SendOptions>,
    ) -> Result<crate::client::messaging::SendMessageUiResult, SendError> {
        use crate::client::messaging::SendMessageUiResult;

        let mut result = match self
            .send_message_attempt(
                attempt,
                conversation,
                text,
                pending_attachments,
                quote,
                options,
            )
            .await
        {
            Ok(result) => result,
            Err((error, row_id)) => {
                let stored = row_id.as_deref()
                    .and_then(|id| id.parse().ok())
                    .and_then(|id| self.get_message_local(&id));
                let Some(message) = stored else {
                    return Err(if row_id.is_some() {
                        SendError::DeliveryUnconfirmed
                    } else {
                        error
                    });
                };
                crate::services::gateway::SendResult {
                    sent_at: message.sent_at,
                    attachment_ids: message.attachments.clone(),
                    message: Some(message),
                    gif_attachment_ids: Vec::new(),
                    bootstrap_initiated: false,
                    anon_message_id: None,
                    send_error: None,
                }
            }
        };

        // No receipt does not prove non-delivery. Keep the original row
        // pending, with retry locked by its durable attempt journal.
        let known_failed = result
            .message
            .as_ref()
            .is_some_and(|m| m.delivery_status == "failed");
        let unresolved = !known_failed
            && !result.bootstrap_initiated
            && (result.send_error.is_some()
                || result
                    .message
                    .as_ref()
                    .is_some_and(|m| matches!(m.delivery_status.as_str(), "failed" | "sending")));
        result.send_error.take();
        let failure_notice = if known_failed {
            Some(
                result
                    .message
                    .as_ref()
                    .and_then(|m| m.failure_reason.clone())
                    .unwrap_or_else(|| "Message was not submitted. Tap to retry.".to_owned()),
            )
        } else {
            unresolved.then(|| SendError::DeliveryUnconfirmed.to_string())
        };

        let real_message_id = result.message_id_string();
        let (message_id_effective, delivery_status) = if known_failed {
            (
                real_message_id,
                crate::services::message::display_delivery_status(
                    "failed",
                    result
                        .message
                        .as_ref()
                        .and_then(|m| m.failure_reason.as_deref()),
                ),
            )
        } else if result.bootstrap_initiated {
            (queued_bubble_id(real_message_id, result.sent_at), "sending")
        } else if failure_notice.is_some()
            || result
                .message
                .as_ref()
                .is_some_and(|m| matches!(m.delivery_status.as_str(), "failed" | "sending"))
        {
            let reason = result
                .message
                .as_ref()
                .and_then(|m| m.failure_reason.as_deref());
            (
                real_message_id,
                crate::services::message::display_delivery_status("sending", reason),
            )
        } else {
            (real_message_id, "sent")
        };

        let timestamp_display = chrono::DateTime::from_timestamp_nanos(result.sent_at as i64)
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();

        // Bootstrap sends carry attachments only after the key exchange, so
        // report an empty list for the "sending" placeholder. Non-bootstrap
        // sends resolve each attachment id against the local store.
        let attachments: Vec<_> = if result.bootstrap_initiated {
            Vec::new()
        } else {
            result
                .attachment_ids
                .iter()
                .filter_map(|id| self.attachments().get(id).ok().flatten())
                .collect()
        };

        // Resolve the quoted parent's media once so the sender's own bubble
        // renders the audio player / preview immediately (not text until reload).
        let reply_parent_media = result
            .message
            .as_ref()
            .map(|m| self.reply_parent_media(m))
            .unwrap_or_default();

        // Prefer the wire quote thumbnail; when it was capped/dropped (>12KB,
        // e.g. big images / gifs) fall back to the parent's LOCAL thumbnail so
        // the sender's own bubble shows the preview immediately instead of a
        // "📷 Photo" label until reload (matches the reloaded canonical message).
        let reply_thumbnail_bytes = result
            .message
            .as_ref()
            .and_then(|m| m.reply_to_thumbnail.clone())
            .or_else(|| reply_parent_media.thumbnail.clone());

        Ok(SendMessageUiResult {
            message_id_effective,
            sent_at: result.sent_at,
            timestamp_display,
            delivery_status,
            bootstrap_initiated: result.bootstrap_initiated,
            attachments,
            reply_thumbnail_bytes,
            reply_parent_media,
            failure_notice,
            failure_reason: result
                .message
                .as_ref()
                .and_then(|m| m.failure_reason.clone()),
        })
    }

    /// Retry a failed channel or DM message and return the updated row.
    /// Channels may acquire a server message ID; DMs retain their original identity.
    pub async fn retry_failed_message(
        &self,
        message_id: &str,
    ) -> Result<crate::domain::message::Message, SendError> {
        use crate::services::message::MessageError;
        let id = message_id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| {
                SendError::Message(MessageError::InvalidContent("Invalid message ID".into()))
            })?;
        let service = &self.services.message_service;
        let message = service
            .get_message_local(&id)?
            .ok_or(MessageError::MessageNotFound(id))?;
        let conversation = ActiveConversation::from_routing_key(
            &message.conversation_type,
            &message.conversation_id.to_string(),
        )
        .ok_or(SendError::UnsupportedAction)?;
        if matches!(
            message.conversation_type,
            crate::domain::message::ConversationType::AnonDm
                | crate::domain::message::ConversationType::AnonGroup
        ) {
            let owner = self
                .session_current_user_id()
                .ok_or(SendError::UnsupportedAction)?;
            if crate::services::gateway::submission::retained_row(
                &self.services.database,
                &owner.to_string(),
                message_id,
            ) {
                if self.retryable_message_timestamp(&id).is_none() {
                    return Err(SendError::UnsupportedAction);
                }
                crate::services::gateway::submission::retry_pending(
                    &self.services.database,
                    &self.services.grpc_pool,
                    &owner.to_string(),
                    Some(message_id),
                    None,
                )
                .await
                .map_err(MessageError::Storage)?;
                let updated = service
                    .get_message_local(&id)?
                    .ok_or(MessageError::MessageNotFound(id))?;
                return if matches!(
                    updated.delivery_status.as_str(),
                    "sent" | "delivered" | "read"
                ) {
                    Ok(updated)
                } else {
                    Err(SendError::DeliveryUnconfirmed)
                };
            }
        }
        if message.conversation_type == crate::domain::message::ConversationType::AnonDm {
            if self.retryable_message_timestamp(&id).is_none() {
                return Err(SendError::UnsupportedAction);
            }
            return self.retry_failed_anon_dm(message, conversation).await;
        }
        if !matches!(
            message.conversation_type,
            crate::domain::message::ConversationType::Channel
                | crate::domain::message::ConversationType::ServerDm
        ) {
            return Err(SendError::UnsupportedAction);
        }
        let session = crate::services::gateway::submission::SendSession::capture(self.auth_state());
        let (key, attempt) = self.retain_retry_attempt(conversation, &message)?;
        let (result, route_started, retry_row) = crate::services::gateway::submission::SEND_SESSION
            .scope(
                session,
                crate::services::gateway::submission::track_submission(
                    async {
                        if message.conversation_type
                            == crate::domain::message::ConversationType::Channel
                        {
                            service.retry_channel_message(&id).await
                        } else {
                            match service.retry_server_dm(&id).await {
                                Ok(_) => service
                                    .get_message_local(&id)?
                                    .ok_or(MessageError::MessageNotFound(id)),
                                Err(error) => Err(error),
                            }
                        }
                    },
                    Some((
                        self.services.database.clone(),
                        key.clone(),
                        attempt.to_string(),
                    )),
                ),
            )
            .await;
        match result {
            Ok(mut updated) => {
                let confirmed = matches!(
                    updated.delivery_status.as_str(),
                    "sent" | "delivered" | "read"
                );
                // A friend DM retry that returned is settled by its owner (the
                // sealed DM outbox or the Shield bootstrap); relabelling could
                // revive a row the outbox just settled `failed`.
                if !confirmed
                    && updated.conversation_type
                        != crate::domain::message::ConversationType::ServerDm
                {
                    updated = service.finish_retry_row(
                        &updated.id,
                        "sending",
                        Some("Delivery unconfirmed"),
                    )?;
                }
                self.finish_send_attempt(&key, attempt, Some(updated.id.to_string()), confirmed)
                    .map_err(|_| SendError::DeliveryUnconfirmed)?;
                Ok(updated)
            }
            Err(error) => {
                if !route_started {
                    self.finish_send_attempt(&key, attempt, None, true)?;
                    return Err(error.into());
                }
                // Retain the original exact identity even after partial fanout,
                // response loss or a failure while persisting the receipt.
                // Preserve a receipt that arrived concurrently with the error.
                if service.get_message_local(&id)?.is_some() {
                    service.finish_retry_row(&id, "sending", Some("Delivery unconfirmed"))?;
                }
                self.finish_send_attempt(
                    &key,
                    attempt,
                    retry_row.or_else(|| Some(id.to_string())),
                    false,
                )
                .map_err(|_| SendError::DeliveryUnconfirmed)?;
                Err(SendError::DeliveryUnconfirmed)
            }
        }
    }

    async fn retry_failed_anon_dm(
        &self,
        message: crate::domain::message::Message,
        conversation: ActiveConversation,
    ) -> Result<crate::domain::message::Message, SendError> {
        let id = message.id;
        let service = &self.services.message_service;
        let session = crate::services::gateway::submission::SendSession::capture(self.auth_state());
        let (key, attempt) = self.retain_retry_attempt(conversation, &message)?;
        let (result, started, _) = crate::services::gateway::submission::SEND_SESSION.scope(
            session,
            crate::services::gateway::submission::track_submission(
                self.services.anon_dm_service.retry_failed_message(&message),
                Some((self.services.database.clone(), key.clone(), attempt.to_string())),
            ),
        ).await;
        match result {
            Ok(()) => {
                let updated = service.finish_retry_row(&id, "sent", None)?;
                self.finish_send_attempt(&key, attempt, Some(id.to_string()), true)
                    .map_err(|_| SendError::DeliveryUnconfirmed)?;
                Ok(updated)
            }
            Err(error) => {
                let error = send_attempt::settle_failure(
                    &self.services.database, &key, attempt, started,
                    Some(id.to_string()), SendError::AnonDm(error),
                )?;
                if matches!(error, SendError::DeliveryUnconfirmed) {
                    service.finish_retry_row(&id, "sending", Some("Delivery unconfirmed"))?;
                }
                Err(error)
            }
        }
    }

    /// Get a [`ConversationHandle`] for per-conversation operations.
    pub fn conversation(self: &Arc<Self>, conv: ActiveConversation) -> ConversationHandle {
        ConversationHandle::new(conv, self.clone())
    }

    /// Mute or unmute a conversation's notifications.
    pub fn mute_conversation(
        self: &Arc<Self>,
        conv: ActiveConversation,
        muted: bool,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).mute_conversation(muted)
    }

    /// Read a conversation's persisted mute state (inverse of
    /// [`Self::mute_conversation`]). Lets the UI reflect the real stored mute
    /// state (e.g. when reopening channel settings) instead of a stale bool.
    pub fn is_conversation_muted(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> Result<bool, crate::services::conversation::ConversationError> {
        self.conversation(conv).is_muted()
    }

    /// Leave a channel or private group. See `ConversationHandle::leave`.
    pub async fn leave_conversation(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).leave().await
    }

    /// Remove a member from a channel or private group. See
    /// `ConversationHandle::remove_member`.
    pub async fn remove_conversation_member(
        self: &Arc<Self>,
        conv: ActiveConversation,
        user: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).remove_member(user).await
    }

    /// Make `new_owner` the owner of a channel or private group. See
    /// `ConversationHandle::transfer_ownership`.
    pub async fn transfer_conversation_ownership(
        self: &Arc<Self>,
        conv: ActiveConversation,
        new_owner: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).transfer_ownership(new_owner).await
    }

    /// Block or unblock a private group. See `ConversationHandle::set_blocked`.
    pub fn set_conversation_blocked(
        self: &Arc<Self>,
        conv: ActiveConversation,
        blocked: bool,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).set_blocked(blocked)
    }

    /// Block or unblock a member within a private group. See
    /// `ConversationHandle::set_member_blocked`.
    pub fn set_conversation_member_blocked(
        self: &Arc<Self>,
        conv: ActiveConversation,
        user: &crate::domain::ids::UserId,
        blocked: bool,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).set_member_blocked(user, blocked)
    }

    /// Delete a conversation from local storage. A secret chat is ended for
    /// both sides first (fails closed when the peer cannot be told).
    pub async fn delete_conversation(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv).delete_conversation().await
    }

    /// Set the disappearing message timer for a conversation.
    ///
    /// Pass `0` to disable. The value is in seconds. See
    /// `ConversationHandle::set_disappearing_timer`.
    pub async fn set_disappearing_timer(
        self: &Arc<Self>,
        conv: ActiveConversation,
        seconds: u32,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv)
            .set_disappearing_timer(seconds)
            .await
    }

    /// Set message lifetime and a separate activation window (zero means until disabled).
    pub async fn set_disappearing_window(
        self: &Arc<Self>,
        conv: ActiveConversation,
        seconds: u32,
        activation_seconds: u32,
    ) -> Result<(), crate::services::conversation::ConversationError> {
        self.conversation(conv)
            .set_disappearing_window(seconds, activation_seconds)
            .await
    }

    pub fn disappearing_state(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> crate::services::disappearing::DmTimer {
        self.conversation(conv).disappearing_state()
    }

    /// A conversation's disappearing-messages timer in seconds (`0` = off).
    pub fn disappearing_timer(self: &Arc<Self>, conv: ActiveConversation) -> u32 {
        self.conversation(conv).disappearing_timer()
    }

    /// Whether the user can change `conv`'s timer (friend DMs only).
    pub fn can_set_disappearing_timer(self: &Arc<Self>, conv: ActiveConversation) -> bool {
        self.conversation(conv).can_set_disappearing_timer()
    }

    /// Conversations (by storage key) whose disappearing timer just changed,
    /// from either side: fires when the change's notice is filed, and when a
    /// friend-DM message arrives stating a timer that differs from the last one
    /// seen here (a lost update healed by the next message — its notice is
    /// filed by the gateway, which has no event bus to announce it on).
    pub fn subscribe_disappearing_timer_changes(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::domain::ids::ChannelId> {
        use crate::domain::message::{ConversationType, MessageContent, SystemMessage};
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let mut src = self.services.event_bus.subscribe_domain();
        let settings = self.channels().settings_repo.clone();
        tokio::spawn(async move {
            let mut seen: std::collections::HashMap<crate::domain::ids::ChannelId, u32> =
                std::collections::HashMap::new();
            loop {
                match src.recv().await {
                    Ok(DomainEvent::MessageReceived { message })
                        if matches!(message.conversation_type, ConversationType::ServerDm | ConversationType::AnonDm) =>
                    {
                        let peer = crate::domain::ids::UserId::from_uuid(
                            *message.conversation_id.as_uuid(),
                        );
                        let now = if message.conversation_type == ConversationType::AnonDm {
                            let id = crate::domain::anon_dm::AnonDmId::from_uuid(*message.conversation_id.as_uuid());
                            crate::services::disappearing::anon_dm_state(&settings, &id)
                                .effective_seconds(crate::util::time::TimestampNanos::now().as_u64())
                        } else {
                            crate::services::disappearing::server_dm_timer(&settings, &peer)
                        };
                        let notice = matches!(
                            message.content,
                            MessageContent::System(SystemMessage::DisappearingTimerChanged { .. })
                        );
                        let changed = seen.insert(message.conversation_id, now) != Some(now);
                        if (notice || changed) && tx.send(message.conversation_id).is_err() {
                            break;
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Messages the disappearing-messages reaper has just deleted from this
    /// device, one batch per sweep, for the open chat to drop.
    pub fn subscribe_expired_messages(
        &self,
    ) -> tokio::sync::broadcast::Receiver<ExpiredMessages> {
        self.expired_messages.subscribe()
    }

}
