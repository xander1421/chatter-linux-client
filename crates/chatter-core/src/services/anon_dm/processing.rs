use crate::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, MessageContent};
use crate::domain::UserId;
use crate::proto::content_v1::Content;
use crate::services::AttachmentService;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Process an incoming message from an anonymous DM conversation.
    ///
    /// This is the service-layer entry point for handling incoming `InboxEvent::AnonDmMessage`.
    /// It validates the conversation, routes based on content type, and returns domain events.
    pub fn process_incoming_content(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        inner_content: &Content,
    ) -> Result<Vec<crate::inbox::handlers::DomainEvent>, AnonDmError> {
        use crate::inbox::handlers::DomainEvent;
        use crate::proto::content_v1::{content::Content as ContentType, typing_message, receipt_message};

        // Check if sender is globally blocked
        if self.is_user_blocked(sender_id).unwrap_or(false) {
            tracing::info!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                "[ANON_DM] Dropped message from blocked user"
            );
            return Ok(vec![]);
        }

        // Check if conversation is blocked
        if self.is_conversation_blocked(conversation_id).unwrap_or(false) {
            tracing::info!(
                conversation_id = %conversation_id,
                "[ANON_DM] Dropped message for blocked conversation"
            );
            return Ok(vec![]);
        }

        // Verify conversation exists and is active
        match self.get_conversation(conversation_id) {
            Ok(Some(dm)) => {
                if dm.peer_id != *sender_id {
                    tracing::info!(
                        sender_id = %sender_id,
                        conversation_id = %conversation_id,
                        expected_peer_id = %dm.peer_id,
                        "[ANON_DM] Dropped message from unexpected sender"
                    );
                    return Ok(vec![]);
                }
                if !dm.is_ready() {
                    tracing::info!(
                        conversation_id = %conversation_id,
                        status = ?dm.status,
                        has_peer_mailbox = dm.peer_mailbox_pk.is_some(),
                        "[ANON_DM] Dropped message in non-active conversation"
                    );
                    return Ok(vec![]);
                }
            }
            Ok(None) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    "[ANON_DM] Dropped message for unknown conversation"
                );
                return Ok(vec![]);
            }
            Err(e) => {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    error = %e,
                    "[ANON_DM] Failed to check conversation"
                );
                return Ok(vec![]);
            }
        }

        let mut events = Vec::new();

        // Log the incoming content type for debugging
        let content_type_name = match &inner_content.content {
            Some(ContentType::DataMessage(_)) => "DataMessage",
            Some(ContentType::Typing(_)) => "Typing",
            Some(ContentType::Reaction(_)) => "Reaction",
            Some(ContentType::Receipt(_)) => "Receipt",
            Some(ContentType::Pin(_)) => "Pin",
            Some(ContentType::Edit(_)) => "Edit",
            Some(ContentType::AnonDmPresence(_)) => "AnonDmPresence",
            Some(ContentType::AnonDmProfileUpdate(_)) => "AnonDmProfileUpdate",
            Some(ContentType::AnonGroupInvite(_)) => "AnonGroupInvite",
            Some(ContentType::AnonGroupJoinAck(_)) => "AnonGroupJoinAck",
            Some(ContentType::AnonGroupMemberList(_)) => "AnonGroupMemberList",
            Some(ContentType::AnonCallOffer(_)) => "AnonCallOffer",
            Some(ContentType::AnonCallAnswer(_)) => "AnonCallAnswer",
            Some(ContentType::AnonCallHangup(_)) => "AnonCallHangup",
            _ => "Unknown",
        };

        tracing::info!(
            conversation_id = %conversation_id,
            sender_id = %sender_id,
            content_type = content_type_name,
            "[ANON_DM_PROCESS] Processing incoming content"
        );

        // Route based on inner content type
        match &inner_content.content {
            Some(ContentType::AnonDmPresence(presence)) => {
                use crate::proto::content_v1::anon_dm_presence;
                use crate::proto::chat_v1::UserPresence;

                let user_presence = match anon_dm_presence::Status::try_from(presence.status).ok() {
                    Some(anon_dm_presence::Status::Online) => UserPresence::Online,
                    Some(anon_dm_presence::Status::Away) => UserPresence::Away,
                    Some(anon_dm_presence::Status::DoNotDisturb) => UserPresence::DoNotDisturb,
                    Some(anon_dm_presence::Status::Offline) | _ => UserPresence::Offline,
                };

                events.push(DomainEvent::UserPresenceChanged {
                    user_id: *sender_id,
                    presence: user_presence,
                    timestamp: presence.timestamp,
                });
            }
            Some(ContentType::AnonDmProfileUpdate(profile_update)) => {
                use crate::crypto::profile_crypto;

                // Validate sender is part of the conversation (double-check already done above but keep for consistency)
                if let Ok(Some(mut conv)) = self.anon_dm_repo.get_conversation(conversation_id) {
                    // Update display name
                    if let Some(name) = &profile_update.display_name {
                        conv.peer_display_name = Some(name.clone());
                    }

                    // Parse profile key and decrypt avatar
                    let mut decrypted_avatar = None;
                    let profile_key: Option<[u8; 32]> = profile_update.profile_key.as_ref().and_then(|pk| {
                        if pk.len() == 32 { Some(pk.as_slice().try_into().unwrap()) } else { None }
                    });

                    if let (Some(pk), Some(encrypted)) = (&profile_key, &profile_update.encrypted_avatar) {
                        if !encrypted.is_empty() {
                            match profile_crypto::decrypt_avatar(pk, encrypted) {
                                Ok(avatar) => {
                                    tracing::info!(
                                        avatar_size = avatar.len(),
                                        "Decrypted peer avatar from profile update"
                                    );
                                    decrypted_avatar = Some(avatar.clone());
                                    conv.peer_avatar = Some(avatar);
                                }
                                Err(e) => {
                                    tracing::debug!("Failed to decrypt avatar in profile update: {}", e);
                                }
                            }
                        }
                    }

                    // Store the updated conversation
                    if let Err(e) = self.anon_dm_repo.update_conversation(&conv) {
                        tracing::warn!("Failed to update anon DM conversation with new profile: {}", e);
                    }

                    // Emit event for UI update
                    events.push(DomainEvent::AnonDmProfileUpdated {
                        conversation_id: *conversation_id,
                        user_id: *sender_id,
                        display_name: profile_update.display_name.clone(),
                        avatar: decrypted_avatar,
                        avatar_url: profile_update.avatar_url.clone(),
                        bio: profile_update.bio.clone(),
                        timestamp: profile_update.timestamp,
                    });
                }
            }
            Some(ContentType::DataMessage(data_msg)) => {
                // Check for delete first
                if let Some(delete) = &data_msg.delete {
                    let deleted = self.delete_message_by_timestamp(
                        conversation_id,
                        sender_id,
                        delete.target_sent_timestamp,
                    )?;

                    if deleted {
                        tracing::info!(
                            "Deleted message at timestamp {} from {} in anon DM {}",
                            delete.target_sent_timestamp,
                            sender_id,
                            conversation_id
                        );
                    }

                    events.push(DomainEvent::MessageDeleted {
                        message_id: MessageId::from_timestamp(delete.target_sent_timestamp),
                        author_id: *sender_id,
                        channel_id: Some((*conversation_id).into()),
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                    return Ok(events);
                }

                // Handle text and/or attachments
                let has_text = !data_msg.body.is_empty();
                let has_attachments = !data_msg.attachments.is_empty();

                if has_text || has_attachments {
                    // Create attachment records from pointers if any
                    let attachment_ids = if has_attachments {
                        let attachment_service = AttachmentService::new_local(self.attachment_repo.clone());
                        let ids = attachment_service.create_from_pointers(&data_msg.attachments);
                        tracing::info!(
                            conversation_id = %conversation_id,
                            attachment_count = ids.len(),
                            "[ANON_DM] Created attachment records from pointers"
                        );
                        ids
                    } else {
                        Vec::new()
                    };

                    // Use sender's timestamp for consistent ordering across devices
                    let sent_at = inner_content.timestamp;

                    // Store message (with or without quote metadata)
                    let quote = crate::inbox::handlers::base_anon::QuoteContext::from_content(inner_content);
                    let message_id = self.store_message_with_attachments(
                        conversation_id,
                        sender_id,
                        &data_msg.body,
                        &attachment_ids,
                        sent_at,
                        quote,
                    )?;
                    tracing::info!(
                        conversation_id = %conversation_id,
                        message_id = %message_id,
                        sent_at = sent_at,
                        has_text = has_text,
                        attachment_count = attachment_ids.len(),
                        "[ANON_DM] Stored message"
                    );

                    // Emit unified MessageReceived event
                    let message = crate::inbox::handlers::base_anon::build_message_from_content(
                        message_id.as_str(),
                        (*conversation_id).into(),
                        ConversationType::AnonDm,
                        *sender_id,
                        &inner_content,
                        sent_at,
                    );
                    events.push(DomainEvent::MessageReceived { message });
                    
                    // Clear typing indicator when message arrives
                    // This prevents stuck typing indicators after message is sent
                    let channel_id: crate::domain::ChannelId = (*conversation_id).into();
                    events.push(DomainEvent::TypingStopped {
                        channel_id,
                        user_id: *sender_id,
                    });
                }
            }
            Some(ContentType::Typing(typing)) => {
                let channel_id: crate::domain::ChannelId = (*conversation_id).into();
                let started = typing.action == typing_message::Action::Started as i32;

                if started {
                    events.push(DomainEvent::TypingStarted {
                        channel_id,
                        user_id: *sender_id,
                    });
                } else {
                    events.push(DomainEvent::TypingStopped {
                        channel_id,
                        user_id: *sender_id,
                    });
                }
            }
            Some(ContentType::Reaction(reaction)) => {
                let target_sender_id: crate::domain::UserId = reaction.target_author_aci.parse().unwrap_or(*sender_id);
                let sent_at_ns = reaction.target_sent_timestamp;

                // Persist reaction to anon_dm_reactions table
                if reaction.remove {
                    match self.remove_reaction(conversation_id, &target_sender_id, sent_at_ns, sender_id, &reaction.emoji) {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                error = %e,
                                "[ANON_DM] Failed to remove reaction from DB"
                            );
                        }
                    }
                    events.push(DomainEvent::ReactionRemoved {
                        sender_id: target_sender_id,
                        sent_at_ns,
                        reactor_id: *sender_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                } else {
                    match self.add_reaction(conversation_id, &target_sender_id, sent_at_ns, sender_id, &reaction.emoji) {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                error = %e,
                                "[ANON_DM] Failed to add reaction to DB"
                            );
                        }
                    }
                    events.push(DomainEvent::ReactionAdded {
                        sender_id: target_sender_id,
                        sent_at_ns,
                        reactor_id: *sender_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                }
            }
            Some(ContentType::Receipt(receipt)) => {
                let channel_id: crate::domain::ChannelId = (*conversation_id).into();
                let receipt_type_name = if receipt.r#type == receipt_message::Type::Read as i32 {
                    "Read"
                } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
                    "Delivery"
                } else {
                    "Unknown"
                };

                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    receipt_type = receipt_type_name,
                    timestamp_count = receipt.timestamps.len(),
                    timestamps = ?receipt.timestamps,
                    "[ANON_DM_RECEIPT_PROCESS] Processing receipt from peer"
                );

                if receipt.r#type == receipt_message::Type::Read as i32 {
                    // Persist read status to anon DM DB
                    let _ = self.update_delivery_status(conversation_id, &receipt.timestamps, "read");

                    events.push(DomainEvent::MessagesRead {
                        reader_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                        channel_id: Some(channel_id),
                    });
                    tracing::info!(
                        conversation_id = %conversation_id,
                        channel_id = %channel_id,
                        timestamp_count = receipt.timestamps.len(),
                        "[ANON_DM_RECEIPT_PROCESS] Emitting MessagesRead event"
                    );
                } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
                    // Persist delivery status to anon DM DB
                    let _ = self.update_delivery_status(conversation_id, &receipt.timestamps, "delivered");

                    events.push(DomainEvent::MessagesDelivered {
                        recipient_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                    });
                    tracing::info!(
                        conversation_id = %conversation_id,
                        timestamp_count = receipt.timestamps.len(),
                        "[ANON_DM_RECEIPT_PROCESS] Emitting MessagesDelivered event"
                    );
                }
            }
            Some(ContentType::Pin(pin)) => {
                let new_pinned = !pin.unpin;

                // Persist pin state to anon DM messages table
                if let Err(e) = self.set_message_pinned(conversation_id, pin.target_sent_timestamp, new_pinned) {
                    tracing::warn!(error = %e, "Failed to persist anon DM pin state");
                }

                // Look up real message ID from DB (not fabricated) for correct pinned_state tracking
                let message_id = self.get_message_by_timestamp(conversation_id, pin.target_sent_timestamp)
                    .ok().flatten()
                    .map(|m| m.id)
                    .unwrap_or_else(|| crate::domain::MessageId::from_timestamp(pin.target_sent_timestamp));
                let channel_id: crate::domain::ChannelId = (*conversation_id).into();

                if pin.unpin {
                    events.push(DomainEvent::MessageUnpinned {
                        message_id,
                        unpinner_id: *sender_id,
                        channel_id,
                        target_sent_timestamp: pin.target_sent_timestamp,
                    });
                } else {
                    events.push(DomainEvent::MessagePinned {
                        message_id,
                        pinner_id: *sender_id,
                        channel_id,
                        target_sent_timestamp: pin.target_sent_timestamp,
                    });
                }
            }
            Some(ContentType::Edit(edit)) => {
                let target_timestamp = edit.target_sent_timestamp;
                let new_text = edit
                    .data_message
                    .as_ref()
                    .map(|dm| dm.body.clone())
                    .unwrap_or_default();

                // Update the message in the database (only if sender is the author)
                match self.update_message_text_by_sender(conversation_id, sender_id, target_timestamp, &new_text) {
                    Ok(true) => {
                        tracing::info!(
                            conversation_id = %conversation_id,
                            sender_id = %sender_id,
                            target_timestamp = target_timestamp,
                            "[ANON_DM] Updated message text (edit from peer)"
                        );
                        events.push(DomainEvent::MessageEdited {
                            message_id: MessageId::from_timestamp(target_timestamp),
                            new_content: MessageContent::Text(new_text),
                            channel_id: (*conversation_id).into(),
                            conversation_type: ConversationType::AnonDm,
                            target_sent_timestamp: target_timestamp,
                        });
                    }
                    Ok(false) => {
                        // Either message not found or sender is not the author
                        tracing::debug!(
                            conversation_id = %conversation_id,
                            sender_id = %sender_id,
                            target_timestamp = target_timestamp,
                            "[ANON_DM] Edit ignored: message not found or sender not author"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = %conversation_id,
                            sender_id = %sender_id,
                            target_timestamp = target_timestamp,
                            error = %e,
                            "[ANON_DM] Failed to update message text"
                        );
                    }
                }
            }
            Some(ContentType::AnonGroupInvite(invite)) => {
                use crate::crypto::profile_crypto;

                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&invite.group_id)
                    .unwrap_or_default();
                let mut pk = [0u8; 32];
                if invite.inviter_mailbox_pk.len() == 32 {
                    pk.copy_from_slice(&invite.inviter_mailbox_pk);
                }

                // Decrypt profile if key is present
                let mut display_name = None;
                let mut avatar = None;
                let profile_key: Option<[u8; 32]> = invite.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 { Some(pk.as_slice().try_into().unwrap()) } else { None }
                });

                if let Some(pk) = &profile_key {
                    if let Some(encrypted) = &invite.encrypted_display_name {
                        display_name = profile_crypto::decrypt_display_name(pk, encrypted).ok();
                    }
                    if let Some(encrypted) = &invite.encrypted_avatar {
                        avatar = profile_crypto::decrypt_avatar(pk, encrypted).ok();
                    }
                }

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    has_profile = profile_key.is_some(),
                    "Processing anonymous group invite in anon DM"
                );

                events.push(DomainEvent::AnonGroupInviteReceived {
                    group_id,
                    group_name: invite.group_name.clone(),
                    inviter_id: *sender_id,
                    inviter_mailbox_pk: pk,
                    profile_key,
                    inviter_display_name: display_name,
                    inviter_avatar: avatar,
                });
            }
            Some(ContentType::AnonGroupJoinAck(ack)) => {
                use crate::crypto::profile_crypto;

                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&ack.group_id)
                    .unwrap_or_default();
                
                // Decrypt profile if key is present
                let mut display_name = None;
                let mut avatar = None;
                let profile_key: Option<[u8; 32]> = ack.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 { Some(pk.as_slice().try_into().unwrap()) } else { None }
                });

                if let Some(pk) = &profile_key {
                    if let Some(encrypted) = &ack.encrypted_display_name {
                        display_name = profile_crypto::decrypt_display_name(pk, encrypted).ok();
                    }
                    if let Some(encrypted) = &ack.encrypted_avatar {
                        avatar = profile_crypto::decrypt_avatar(pk, encrypted).ok();
                    }
                }

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    has_profile = profile_key.is_some(),
                    "Processing anonymous group join ack in anon DM"
                );

                events.push(DomainEvent::AnonGroupMemberJoined {
                    group_id,
                    user_id: *sender_id,
                    profile_key,
                    display_name,
                    avatar,
                });
            }
            Some(ContentType::AnonGroupMemberList(list)) => {
                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&list.group_id)
                    .unwrap_or_default();

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    member_count = list.members.len(),
                    "Processing anonymous group member list in anon DM"
                );

                // Emit event for UI to update local state via AnonGroupService
                events.push(DomainEvent::AnonGroupMemberListReceived {
                    group_id,
                    members: list.members.clone(),
                });
            }
            Some(ContentType::AnonCallOffer(offer)) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    room_name = %offer.room_name,
                    call_type = ?offer.call_type,
                    "[ANON_DM_CALL] Received call offer"
                );

                // Emit event for UI to show incoming call
                events.push(DomainEvent::AnonCallOfferReceived {
                    conversation_id: *conversation_id,
                    sender_id: *sender_id,
                    call_room_id: offer.call_room_id.clone(),
                    room_name: offer.room_name.clone(),
                    call_type: offer.call_type,
                    caller_display_name: offer.caller_display_name.clone(),
                    call_encryption_key: offer.call_encryption_key.clone(),
                    timestamp: inner_content.timestamp,
                });
            }
            Some(ContentType::AnonCallAnswer(answer)) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    accepted = answer.accepted,
                    "[ANON_DM_CALL] Received call answer"
                );

                // Emit event for UI to handle call acceptance/rejection
                events.push(DomainEvent::AnonCallAnswerReceived {
                    conversation_id: *conversation_id,
                    sender_id: *sender_id,
                    call_room_id: answer.call_room_id.clone(),
                    accepted: answer.accepted,
                    callee_display_name: answer.callee_display_name.clone(),
                    timestamp: inner_content.timestamp,
                });
            }
            Some(ContentType::AnonCallHangup(hangup)) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    reason = ?hangup.reason,
                    "[ANON_DM_CALL] Received call hangup"
                );

                // Emit event for UI to end call
                events.push(DomainEvent::AnonCallHangupReceived {
                    conversation_id: *conversation_id,
                    sender_id: *sender_id,
                    call_room_id: hangup.call_room_id.clone(),
                    reason: hangup.reason,
                    timestamp: inner_content.timestamp,
                });
            }
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "[ANON_DM] Ignoring unhandled inner content type"
                );
            }
        }

        Ok(events)
    }

    /// Store a data message received from the peer (wire-format `content_type` + `content_json`).
    ///
    /// `content_json` is the `{"text": "..."}` envelope produced by the caller.
    /// `message_content_from_type_and_json` unwraps it before persisting so the
    /// row stores real `MessageContent` (not the raw JSON string).
    pub fn store_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
        quote: Option<crate::inbox::handlers::base_anon::QuoteContext>,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        let content = crate::services::anon_shared::message_content_from_type_and_json(
            content_type,
            content_json,
        );
        let message = crate::services::anon_shared::build_anon_message(
            channel_id,
            ConversationType::AnonDm,
            *sender_id,
            content,
            Vec::new(),
            sent_at,
            quote,
        );
        let message_id = message.id;
        self.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored message {} in conversation {} with sent_at={}",
            message_id,
            conversation_id,
            sent_at
        );
        Ok(AnonDmMessageId::from_string(message_id.to_string()))
    }

    /// Store a text message.
    pub fn store_text_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        let content = crate::services::anon_shared::message_content_from_text_and_attachments(
            text,
            &[],
        );
        let message = crate::services::anon_shared::build_anon_message(
            channel_id,
            ConversationType::AnonDm,
            *sender_id,
            content,
            Vec::new(),
            sent_at,
            None,
        );
        let message_id = message.id;
        self.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored text message {} in conversation {} with sent_at={}",
            message_id,
            conversation_id,
            sent_at
        );
        Ok(AnonDmMessageId::from_string(message_id.to_string()))
    }

    /// Store a message with text and/or attachments.
    ///
    /// `quote` carries reply metadata (target message id, author, preview) extracted
    /// from the inbound Content proto. Persisting it on the row is what keeps reply
    /// bubbles visible after a channel reload on the receiver side.
    pub fn store_message_with_attachments(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        attachment_ids: &[crate::domain::AttachmentId],
        sent_at: u64,
        quote: Option<crate::inbox::handlers::base_anon::QuoteContext>,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        let content = crate::services::anon_shared::message_content_from_text_and_attachments(
            text,
            attachment_ids,
        );
        let message = crate::services::anon_shared::build_anon_message(
            channel_id,
            ConversationType::AnonDm,
            *sender_id,
            content,
            attachment_ids.to_vec(),
            sent_at,
            quote,
        );
        let message_id = message.id;
        let has_text = !text.is_empty();

        self.message_repo.insert(&message)?;
        tracing::debug!(
            conversation_id = %conversation_id,
            message_id = %message_id,
            sent_at = sent_at,
            has_text = has_text,
            attachment_count = attachment_ids.len(),
            "[ANON_DM] store_message_with_attachments completed"
        );
        Ok(AnonDmMessageId::from_string(message_id.to_string()))
    }

    /// Mark a conversation as read.
    pub fn mark_as_read(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        Ok(self.anon_dm_repo.mark_as_read(conversation_id)?)
    }

    /// Update delivery status for messages by timestamps.
    pub fn update_delivery_status(
        &self,
        conversation_id: &AnonDmId,
        timestamps: &[u64],
        status: &str,
    ) -> Result<usize, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.update_delivery_status_by_timestamps(&channel_id, &ConversationType::AnonDm, timestamps, status)?)
    }

    /// Update message text (for handling edits from peer).
    ///
    /// Only updates if sender_id matches the message author (security).
    /// Returns `Ok(true)` if a message was updated, `Ok(false)` if message not found
    /// or sender doesn't match author.
    pub fn update_message_text_by_sender(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        sent_timestamp: u64,
        new_text: &str,
    ) -> Result<bool, AnonDmError> {
        // Verify the sender is the author by looking up the message first
        let channel_id: ChannelId = (*conversation_id).into();
        if let Some(existing) = self.message_repo.get_by_conversation_sender_timestamp(&channel_id, sender_id, sent_timestamp)? {
            // Sender matches - update the text
            self.message_repo.update_text_by_timestamp(&channel_id, &ConversationType::AnonDm, sent_timestamp, new_text)?;
            let _ = existing; // Confirm message existed and sender matched
            Ok(true)
        } else {
            // Message not found or sender doesn't match
            Ok(false)
        }
    }
}
