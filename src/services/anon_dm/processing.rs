use chrono::Utc;
use crate::domain::anon_dm::{AnonDmId, AnonDmMessage as DomainAnonDmMessage, AnonDmMessageId};
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
            Some(ContentType::AnonGroupInvite(_)) => "AnonGroupInvite",
            Some(ContentType::AnonGroupJoinAck(_)) => "AnonGroupJoinAck",
            Some(ContentType::AnonGroupMemberList(_)) => "AnonGroupMemberList",
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

                    events.push(DomainEvent::AnonDmMessageDeleted {
                        conversation_id: *conversation_id,
                        message_timestamp: delete.target_sent_timestamp,
                    });
                    return Ok(events);
                }

                // Handle text and/or attachments
                let has_text = !data_msg.body.is_empty();
                let has_attachments = !data_msg.attachments.is_empty();

                if has_text || has_attachments {
                    // Create attachment records from pointers if any
                    let attachment_ids = if has_attachments {
                        let attachment_service = AttachmentService::new(self.attachment_repo.clone());
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
                    let message_id = self.store_message_with_attachments(
                        conversation_id,
                        sender_id,
                        &data_msg.body,
                        &attachment_ids,
                        sent_at,
                    )?;
                    tracing::info!(
                        conversation_id = %conversation_id,
                        message_id = %message_id,
                        sent_at = sent_at,
                        has_text = has_text,
                        attachment_count = attachment_ids.len(),
                        "[ANON_DM] Stored message"
                    );

                    events.push(DomainEvent::AnonDmMessageReceived {
                        conversation_id: *conversation_id,
                        sender_id: *sender_id,
                        message_id,
                        inner_content: inner_content.clone(),
                        sent_at,
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
                let message_id = crate::domain::MessageId::from_timestamp(reaction.target_sent_timestamp);
                let author_id = reaction.target_author_aci.parse().unwrap_or(*sender_id);

                if reaction.remove {
                    events.push(DomainEvent::ReactionRemoved {
                        message_id,
                        author_id,
                        reactor_id: *sender_id,
                        emoji: reaction.emoji.clone(),
                        timestamp: reaction.target_sent_timestamp,
                    });
                } else {
                    events.push(DomainEvent::ReactionAdded {
                        message_id,
                        author_id,
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
                let message_id = crate::domain::MessageId::from_timestamp(pin.target_sent_timestamp);
                let channel_id: crate::domain::ChannelId = (*conversation_id).into();

                if pin.unpin {
                    events.push(DomainEvent::MessageUnpinned {
                        message_id,
                        unpinner_id: *sender_id,
                        channel_id,
                    });
                } else {
                    events.push(DomainEvent::MessagePinned {
                        message_id,
                        pinner_id: *sender_id,
                        channel_id,
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
                        events.push(DomainEvent::AnonDmMessageEdited {
                            conversation_id: *conversation_id,
                            message_timestamp: target_timestamp,
                            new_text,
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
                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&invite.group_id)
                    .unwrap_or_default();
                let mut pk = [0u8; 32];
                if invite.inviter_mailbox_pk.len() == 32 {
                    pk.copy_from_slice(&invite.inviter_mailbox_pk);
                }

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Processing anonymous group invite in anon DM"
                );

                events.push(DomainEvent::AnonGroupInviteReceived {
                    group_id,
                    group_name: invite.group_name.clone(),
                    inviter_id: *sender_id,
                    inviter_mailbox_pk: pk,
                    profile_key: None,
                    inviter_display_name: None,
                    inviter_avatar: None,
                });
            }
            Some(ContentType::AnonGroupJoinAck(ack)) => {
                let group_id = crate::domain::anon_group::AnonGroupId::try_from_slice(&ack.group_id)
                    .unwrap_or_default();
                
                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Processing anonymous group join ack in anon DM"
                );

                events.push(DomainEvent::AnonGroupMemberJoined {
                    group_id,
                    user_id: *sender_id,
                    profile_key: None,
                    display_name: None,
                    avatar: None,
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
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "[ANON_DM] Ignoring unhandled inner content type"
                );
            }
        }

        Ok(events)
    }

    /// Store a message received from the peer.
    pub fn store_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let message = DomainAnonDmMessage {
            id: AnonDmMessageId::new(),
            conversation_id: *conversation_id,
            sender_id: *sender_id,
            content_type,
            content_json: content_json.to_string(),
            attachments_blob: vec![8, 0, 0, 0, 0, 0, 0, 0], // Empty vec bincode
            is_deleted: false,
            is_read: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
        };

        self.anon_dm_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored message {} in conversation {} with sent_at={}",
            message.id,
            conversation_id,
            sent_at
        );
        Ok(message.id)
    }

    /// Store a text message.
    pub fn store_text_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let message = DomainAnonDmMessage::new_text(*conversation_id, *sender_id, text, sent_at);
        self.anon_dm_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored text message {} in conversation {} with sent_at={}",
            message.id,
            conversation_id,
            sent_at
        );
        Ok(message.id)
    }

    /// Store a message with text and/or attachments.
    pub fn store_message_with_attachments(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        attachment_ids: &[crate::domain::AttachmentId],
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        use crate::storage::serialization::db_serialize;

        // Determine content type: 1=text, 2=attachment only, 3=mixed
        let has_text = !text.is_empty();
        let has_attachments = !attachment_ids.is_empty();

        let (content_type, content_json) = match (has_text, has_attachments) {
            (true, false) => (1, serde_json::json!({ "text": text }).to_string()),
            (false, true) => (2, serde_json::json!({ "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }).to_string()),
            (true, true) => (3, serde_json::json!({ "text": text, "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }).to_string()),
            (false, false) => (0, "{}".to_string()),
        };

        // Serialize attachment IDs to blob
        let attachment_ids_vec: Vec<_> = attachment_ids.to_vec();
        let attachments_blob = db_serialize(&attachment_ids_vec).unwrap_or_else(|_| vec![8, 0, 0, 0, 0, 0, 0, 0]);

        let message = DomainAnonDmMessage {
            id: AnonDmMessageId::new(),
            conversation_id: *conversation_id,
            sender_id: *sender_id,
            content_type,
            content_json,
            attachments_blob,
            is_deleted: false,
            is_read: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
        };

        self.anon_dm_repo.insert_message(&message)?;
        tracing::info!(
            conversation_id = %conversation_id,
            message_id = %message.id,
            sent_at = sent_at,
            has_text = has_text,
            attachment_count = attachment_ids.len(),
            "[ANON_DM] Stored message (sender)"
        );
        Ok(message.id)
    }

    /// Mark a conversation as read.
    pub fn mark_as_read(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        Ok(self.anon_dm_repo.mark_as_read(conversation_id)?)
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
        Ok(self.anon_dm_repo.update_message_text_by_sender(conversation_id, sender_id, sent_timestamp, new_text)?)
    }
}
