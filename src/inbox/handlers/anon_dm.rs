//! Anonymous DM message handler.
//!
//! Handles incoming anonymous DM messages: requests, acceptances, and messages.
//!
//! Note: This handler only parses messages and emits events. Async operations
//! (like sending acceptance responses) are handled by the UI layer in response
//! to events.

use std::str::FromStr;
use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};
use crate::domain::anon_dm::AnonDmId;
use crate::domain::UserId;
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::domain::ids::ChannelId;
use crate::proto::content_v1::{self, typing_message, receipt_message, anon_dm_presence, Content};
use crate::services::anon_dm::AnonDmService;

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// AnonDmHandler
// ============================================================================

/// Handler for anonymous DM messages.
///
/// Processes incoming message requests (from strangers), acceptances (when our
/// request is accepted), and messages (in active conversations).
///
/// Note: This handler does NOT call async service methods. Instead, it updates
/// local state synchronously and emits events. Any outgoing encrypted messages
/// (like acceptance responses) should be triggered by the UI layer.
pub struct AnonDmHandler {
    service: Arc<AnonDmService>,
}

impl AnonDmHandler {
    /// Create a new anonymous DM handler.
    pub fn new(service: Arc<AnonDmService>) -> Self {
        Self { service }
    }

    /// Extract sender UserId from processed message.
    fn get_sender_id(&self, message: &ProcessedMessage) -> Result<UserId, HandlerError> {
        message
            .sender_address
            .as_ref()
            .and_then(|addr| UserId::from_str(addr.name()).ok())
            .ok_or_else(|| HandlerError::missing_context("sender_address"))
    }

    /// Handle an incoming message request from a stranger.
    fn handle_request(
        &self,
        request: content_v1::AnonDmRequest,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Check if sender is globally blocked
        if self.service.is_user_blocked(&sender_id).unwrap_or(false) {
            tracing::debug!("Dropped message request from blocked user {}", sender_id);
            return Ok(HandlerOutcome::Ignored);
        }

        let conversation_id = AnonDmId::try_from_slice(&request.conversation_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid conversation ID in request"))?;

        let sender_mailbox_pk = if request.initiator_mailbox_pk.len() == 32 {
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&request.initiator_mailbox_pk);
            pk
        } else {
            return Err(HandlerError::parse_failed("Invalid initiator mailbox public key"));
        };

        let profile_key = if let Some(ref pk) = request.profile_key {
            if pk.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(pk);
                Some(key)
            } else {
                None
            }
        } else {
            None
        };

        let intro_message = if request.intro_message.is_empty() {
            None
        } else {
            Some(request.intro_message.as_str())
        };

        // Decrypt display name - required for anon DM requests
        // If missing or decryption fails, reject the request (sender should retry)
        let sender_display_name: String = match (&profile_key, &request.encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => name,
                    Ok(_) => {
                        tracing::debug!("Empty display name after decryption from {}", sender_id);
                        return Ok(HandlerOutcome::Ignored);
                    }
                    Err(e) => {
                        tracing::debug!("Failed to decrypt sender display name from {}: {}", sender_id, e);
                        return Ok(HandlerOutcome::Ignored);
                    }
                }
            }
            _ => {
                tracing::debug!("Missing profile_key or encrypted_display_name from {}", sender_id);
                return Ok(HandlerOutcome::Ignored);
            }
        };

        // Decrypt avatar if present (optional - avatar is not required)
        let sender_avatar: Option<Vec<u8>> = match (&profile_key, &request.encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_avatar(pk, encrypted) {
                    Ok(avatar) => Some(avatar),
                    Err(e) => {
                        tracing::debug!("Failed to decrypt sender avatar from {}: {}", sender_id, e);
                        None
                    }
                }
            }
            _ => None,
        };

        // Clone avatar before passing to service (we need it for the event too)
        let sender_avatar_for_event = sender_avatar.clone();

        // Store the request in the database
        if let Err(e) = self.service.handle_request(
            conversation_id,
            sender_id,
            sender_mailbox_pk,
            intro_message,
            profile_key,
            Some(sender_display_name.clone()),
            sender_avatar,
        ) {
            tracing::warn!("Failed to handle message request from {}: {}", sender_id, e);
            // Don't fail - might be a duplicate
        }

        // Emit event for UI to show message request
        ctx.emit(DomainEvent::AnonDmRequestReceived {
            conversation_id,
            sender_id,
            sender_mailbox_pk,
            intro_message: intro_message.map(|s| s.to_string()),
            profile_key,
            sender_display_name: Some(sender_display_name),
            sender_avatar: sender_avatar_for_event,
        });

        tracing::info!(
            "Received anonymous DM request {} from {}",
            conversation_id,
            sender_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an acceptance of our message request.
    fn handle_accept(
        &self,
        accept: content_v1::AnonDmAccept,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let conversation_id = AnonDmId::try_from_slice(&accept.conversation_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid conversation ID in accept"))?;

        let peer_mailbox_pk = if accept.responder_mailbox_pk.len() == 32 {
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&accept.responder_mailbox_pk);
            pk
        } else {
            return Err(HandlerError::parse_failed("Invalid responder mailbox public key"));
        };

        let profile_key = if let Some(ref pk) = accept.profile_key {
            if pk.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(pk);
                Some(key)
            } else {
                None
            }
        } else {
            None
        };

        // Decrypt display name - required for acceptance (same as request handling)
        // If missing or decryption fails, reject (they should retry)
        let peer_display_name: String = match (&profile_key, &accept.encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => name,
                    Ok(_) => {
                        tracing::debug!("Empty display name after decryption in acceptance from {}", sender_id);
                        return Ok(HandlerOutcome::Ignored);
                    }
                    Err(e) => {
                        tracing::debug!("Failed to decrypt display name in acceptance from {}: {}", sender_id, e);
                        return Ok(HandlerOutcome::Ignored);
                    }
                }
            }
            _ => {
                tracing::debug!(
                    "Acceptance from {} missing profile_key or encrypted_display_name, ignoring",
                    sender_id
                );
                return Ok(HandlerOutcome::Ignored);
            }
        };

        // Decrypt avatar if present (optional - avatar is not required)
        let peer_avatar: Option<Vec<u8>> = match (&profile_key, &accept.encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_avatar(pk, encrypted) {
                    Ok(avatar) => Some(avatar),
                    Err(e) => {
                        tracing::debug!("Failed to decrypt peer avatar in acceptance from {}: {}", sender_id, e);
                        None
                    }
                }
            }
            _ => None,
        };

        // Clone avatar before passing to service (we need it for the event too)
        let peer_avatar_for_event = peer_avatar.clone();

        // Update conversation status
        if let Err(e) = self.service.handle_acceptance(
            conversation_id,
            sender_id,
            peer_mailbox_pk,
            profile_key,
            Some(peer_display_name.clone()),
            peer_avatar,
        ) {
            tracing::warn!(
                "Failed to handle acceptance for {} from {}: {}",
                conversation_id,
                sender_id,
                e
            );
        }

        // Emit event for UI
        ctx.emit(DomainEvent::AnonDmRequestAccepted {
            conversation_id,
            peer_id: sender_id,
            peer_mailbox_pk,
            profile_key,
            peer_display_name: peer_display_name.clone(),
            peer_avatar: peer_avatar_for_event,
        });

        tracing::info!(
            "Received acceptance for conversation {} from {}",
            conversation_id,
            sender_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming message in an active conversation.
    #[allow(clippy::boxed_local)] // Box comes from proto definition for large nested messages
    fn handle_message(
        &self,
        msg: Box<content_v1::AnonDmMessage>,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Check if sender is globally blocked
        if self.service.is_user_blocked(&sender_id).unwrap_or(false) {
            tracing::debug!("Dropped message from blocked user {}", sender_id);
            return Ok(HandlerOutcome::Ignored);
        }

        let conversation_id = AnonDmId::try_from_slice(&msg.conversation_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid conversation ID in message"))?;

        // Check if conversation is blocked
        if self.service.is_conversation_blocked(&conversation_id).unwrap_or(false) {
            tracing::debug!("Dropped message for blocked conversation {}", conversation_id);
            return Ok(HandlerOutcome::Ignored);
        }

        // Verify conversation exists and is active
        match self.service.get_conversation(&conversation_id) {
            Ok(Some(dm)) => {
                if dm.peer_id != sender_id {
                    tracing::debug!(
                        "Dropped message from unexpected sender {} in conversation {}",
                        sender_id,
                        conversation_id
                    );
                    return Ok(HandlerOutcome::Ignored);
                }
                if !dm.is_ready() {
                    tracing::debug!(
                        "Dropped message in non-active conversation {}",
                        conversation_id
                    );
                    return Ok(HandlerOutcome::Ignored);
                }
            }
            Ok(None) => {
                tracing::debug!(
                    "Dropped message for unknown conversation {}",
                    conversation_id
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Err(e) => {
                tracing::warn!("Failed to check conversation {}: {}", conversation_id, e);
                return Ok(HandlerOutcome::Ignored);
            }
        }

        // Extract the inner content
        let inner_content = msg.inner_content.ok_or_else(|| {
            HandlerError::parse_failed("Anonymous DM message missing inner content")
        })?;

        // Route based on inner content type
        match &inner_content.content {
            Some(content_v1::content::Content::Typing(typing)) => {
                self.handle_typing(conversation_id, sender_id, typing, ctx)
            }
            Some(content_v1::content::Content::Reaction(reaction)) => {
                self.handle_reaction(conversation_id, sender_id, reaction, ctx)
            }
            Some(content_v1::content::Content::Receipt(receipt)) => {
                self.handle_receipt(conversation_id, sender_id, receipt, ctx)
            }
            Some(content_v1::content::Content::Pin(pin)) => {
                self.handle_pin(conversation_id, sender_id, pin, ctx)
            }
            Some(content_v1::content::Content::Edit(edit)) => {
                self.handle_edit(conversation_id, sender_id, edit, ctx)
            }
            Some(content_v1::content::Content::DataMessage(data_msg)) => {
                // Check for Delete first
                if let Some(delete) = &data_msg.delete {
                    return self.handle_delete(conversation_id, sender_id, delete.target_sent_timestamp, ctx);
                }
                // Regular data message
                self.handle_data_message(conversation_id, sender_id, &inner_content, ctx)
            }
            Some(content_v1::content::Content::AnonCallOffer(offer)) => {
                self.handle_call_offer(conversation_id, sender_id, offer, ctx)
            }
            Some(content_v1::content::Content::AnonCallAnswer(answer)) => {
                self.handle_call_answer(conversation_id, sender_id, answer, ctx)
            }
            Some(content_v1::content::Content::AnonCallHangup(hangup)) => {
                self.handle_call_hangup(conversation_id, sender_id, hangup, ctx)
            }
            _ => {
                // Unknown or other content types - treat as data message
                self.handle_data_message(conversation_id, sender_id, &inner_content, ctx)
            }
        }
    }

    /// Handle a typing indicator.
    fn handle_typing(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        typing: &content_v1::TypingMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Convert AnonDmId to ChannelId for the event (reuse existing event types)
        let channel_id: ChannelId = conversation_id.into();

        let event = if typing.action == typing_message::Action::Started as i32 {
            DomainEvent::TypingStarted {
                channel_id,
                user_id: sender_id,
            }
        } else {
            DomainEvent::TypingStopped {
                channel_id,
                user_id: sender_id,
            }
        };
        ctx.emit(event);

        tracing::debug!(
            "Received typing indicator from {} in anon DM {}",
            sender_id,
            conversation_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a presence update.
    fn handle_presence(
        &self,
        presence: content_v1::AnonDmPresence,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let conversation_id = AnonDmId::try_from_slice(&presence.conversation_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid conversation ID in presence"))?;

        // Check if sender is globally blocked
        if self.service.is_user_blocked(&sender_id).unwrap_or(false) {
            tracing::debug!("Dropped presence update from blocked user {}", sender_id);
            return Ok(HandlerOutcome::Ignored);
        }

        // Check if conversation is blocked
        if self.service.is_conversation_blocked(&conversation_id).unwrap_or(false) {
            tracing::debug!("Dropped presence update for blocked conversation {}", conversation_id);
            return Ok(HandlerOutcome::Ignored);
        }

        // Map anon_dm_presence::Status to UserPresence
        use std::convert::TryFrom;
        let user_presence = match anon_dm_presence::Status::try_from(presence.status).ok() {
            Some(anon_dm_presence::Status::Online) => crate::proto::UserPresence::Online,
            Some(anon_dm_presence::Status::Away) => crate::proto::UserPresence::Away,
            Some(anon_dm_presence::Status::DoNotDisturb) => crate::proto::UserPresence::DoNotDisturb,
            Some(anon_dm_presence::Status::Offline) | _ => crate::proto::UserPresence::Offline,
        };

        // Emit presence update event
        ctx.emit(DomainEvent::UserPresenceChanged {
            user_id: sender_id,
            presence: user_presence,
            timestamp: presence.timestamp,
        });

        tracing::info!(
            "Received presence update from {} in anon DM {}: {:?}",
            sender_id,
            conversation_id,
            user_presence
        );

        Ok(HandlerOutcome::Processed)
    }

    fn handle_profile_update(
        &self,
        profile_update: content_v1::AnonDmProfileUpdate,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let conversation_id = AnonDmId::try_from_slice(&profile_update.conversation_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid conversation ID in profile update"))?;

        // Check if sender is globally blocked
        if self.service.is_user_blocked(&sender_id).unwrap_or(false) {
            tracing::debug!("Dropped profile update from blocked user {}", sender_id);
            return Ok(HandlerOutcome::Ignored);
        }

        // Check if conversation is blocked
        if self.service.is_conversation_blocked(&conversation_id).unwrap_or(false) {
            tracing::debug!("Dropped profile update for blocked conversation {}", conversation_id);
            return Ok(HandlerOutcome::Ignored);
        }

        // Validate sender is part of the conversation
        let conv = self.service.anon_dm_repo.get_conversation(&conversation_id)
            .map_err(|e| HandlerError::parse_failed(format!("Failed to get conversation: {}", e)))?
            .ok_or_else(|| HandlerError::parse_failed("Unknown conversation for profile update"))?;
        
        if conv.peer_id != sender_id {
            tracing::warn!(
                "Received profile update from {} but peer is {} in conversation {}",
                sender_id, conv.peer_id, conversation_id
            );
            return Ok(HandlerOutcome::Ignored);
        }

        tracing::info!(
            "Received profile update from {} in anon DM {}",
            sender_id, conversation_id
        );

        // Update the cached conversation with new profile info
        if let Ok(Some(mut conv)) = self.service.anon_dm_repo.get_conversation(&conversation_id) {
            // Update peer display name if provided
            if let Some(name) = &profile_update.display_name {
                conv.peer_display_name = Some(name.clone());
            }
            
            // Note: Avatar URL is provided but we don't have a field for URL storage
            // We would need to download and store the avatar bytes in conv.peer_avatar
            // For now, just log that an avatar update was received
            if let Some(avatar_url) = &profile_update.avatar_url {
                tracing::debug!("Received avatar URL update: {}", avatar_url);
                // TODO: Download avatar and store in conv.peer_avatar
            }
            
            // Store the updated conversation
            if let Err(e) = self.service.anon_dm_repo.update_conversation(&conv) {
                tracing::warn!("Failed to update anon DM conversation with new profile: {}", e);
            }
        }

        // Emit event for UI update
        ctx.emit(DomainEvent::AnonDmProfileUpdated {
            conversation_id,
            user_id: sender_id,
            display_name: profile_update.display_name,
            avatar_url: profile_update.avatar_url,
            bio: profile_update.bio,
            timestamp: profile_update.timestamp,
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a reaction.
    fn handle_reaction(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        reaction: &content_v1::Reaction,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // For anon DMs, we emit the standard reaction event
        // The message ID is derived from the target_sent_timestamp
        let message_id = crate::domain::MessageId::from_timestamp(reaction.target_sent_timestamp);
        let author_id = UserId::from_str(&reaction.target_author_aci)
            .unwrap_or(sender_id);

        let event = if reaction.remove {
            DomainEvent::ReactionRemoved {
                message_id,
                author_id,
                reactor_id: sender_id,
                emoji: reaction.emoji.clone(),
                timestamp: reaction.timestamp,
            }
        } else {
            DomainEvent::ReactionAdded {
                message_id,
                author_id,
                reactor_id: sender_id,
                emoji: reaction.emoji.clone(),
                timestamp: reaction.timestamp,
            }
        };
        ctx.emit(event);

        tracing::debug!(
            "Received reaction {} from {} in anon DM {}",
            reaction.emoji,
            sender_id,
            conversation_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a read receipt.
    fn handle_receipt(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        receipt: &content_v1::ReceiptMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        if receipt.r#type == receipt_message::Type::Read as i32 {
            let channel_id: ChannelId = conversation_id.into();
            ctx.emit(DomainEvent::MessagesRead {
                reader_id: sender_id,
                message_timestamps: receipt.timestamps.clone(),
                receipt_timestamp: receipt.timestamp,
                channel_id: Some(channel_id),
            });

            tracing::debug!(
                "Received read receipt for {} messages from {} in anon DM {}",
                receipt.timestamps.len(),
                sender_id,
                conversation_id
            );
        } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
            ctx.emit(DomainEvent::MessagesDelivered {
                recipient_id: sender_id,
                message_timestamps: receipt.timestamps.clone(),
                receipt_timestamp: receipt.timestamp,
            });

            tracing::debug!(
                "Received delivery receipt for {} messages from {} in anon DM {}",
                receipt.timestamps.len(),
                sender_id,
                conversation_id
            );
        }

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a delete-for-everyone message.
    fn handle_delete(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        target_sent_timestamp: u64,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Delete the message from our local store
        match self.service.delete_message_by_timestamp(&conversation_id, &sender_id, target_sent_timestamp) {
            Ok(true) => {
                tracing::info!(
                    "Deleted message at timestamp {} from {} in anon DM {}",
                    target_sent_timestamp,
                    sender_id,
                    conversation_id
                );
            }
            Ok(false) => {
                tracing::debug!(
                    "Message at timestamp {} not found for deletion in anon DM {}",
                    target_sent_timestamp,
                    conversation_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to delete message at timestamp {} in anon DM {}: {}",
                    target_sent_timestamp,
                    conversation_id,
                    e
                );
            }
        }

        // Emit event for UI to update
        let message_id = crate::domain::MessageId::from_timestamp(target_sent_timestamp);
        ctx.emit(DomainEvent::MessageDeleted {
            message_id,
            author_id: sender_id,
            channel_id: Some(conversation_id.into()),
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a pin/unpin message.
    fn handle_pin(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        pin: &content_v1::PinMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let message_id = crate::domain::MessageId::from_timestamp(pin.target_sent_timestamp);
        let channel_id: ChannelId = conversation_id.into();

        let event = if pin.unpin {
            DomainEvent::MessageUnpinned {
                message_id,
                unpinner_id: sender_id,
                channel_id,
                target_sent_timestamp: pin.target_sent_timestamp,
            }
        } else {
            DomainEvent::MessagePinned {
                message_id,
                pinner_id: sender_id,
                channel_id,
                target_sent_timestamp: pin.target_sent_timestamp,
            }
        };
        ctx.emit(event);

        tracing::info!(
            "Received {} for message at timestamp {} in anon DM {}",
            if pin.unpin { "unpin" } else { "pin" },
            pin.target_sent_timestamp,
            conversation_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an edit message.
    ///
    /// Only updates the message if sender_id matches the message author.
    /// This ensures users can only edit their own messages.
    fn handle_edit(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        edit: &content_v1::EditMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let target_timestamp = edit.target_sent_timestamp;
        let new_text = edit
            .data_message
            .as_ref()
            .map(|dm| dm.body.clone())
            .unwrap_or_default();

        // Update the message in the database (only if sender is the author)
        match self.service.update_message_text_by_sender(&conversation_id, &sender_id, target_timestamp, &new_text) {
            Ok(true) => {
                tracing::info!(
                    sender_id = %sender_id,
                    conversation_id = %conversation_id,
                    target_timestamp = target_timestamp,
                    "Updated message text (edit from peer)"
                );
                // Emit event for UI to update
                ctx.emit(DomainEvent::AnonDmMessageEdited {
                    conversation_id,
                    message_timestamp: target_timestamp,
                    new_text,
                });
            }
            Ok(false) => {
                // Either message not found or sender is not the author (security check)
                tracing::debug!(
                    sender_id = %sender_id,
                    conversation_id = %conversation_id,
                    target_timestamp = target_timestamp,
                    "Edit ignored: message not found or sender not author"
                );
            }
            Err(e) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    conversation_id = %conversation_id,
                    target_timestamp = target_timestamp,
                    error = %e,
                    "Failed to update message text"
                );
            }
        }

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a regular data message (text, etc.).
    fn handle_data_message(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        inner_content: &Content,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Determine content type and JSON representation for storage
        let (content_type, content_json) = Self::content_to_storage(inner_content);

        // Use sender's timestamp for consistent ordering across devices
        let sent_at = inner_content.timestamp;

        // Store the message
        let message_id = match self.service.store_message(
            &conversation_id,
            &sender_id,
            content_type,
            &content_json,
            sent_at,
        ) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to store anonymous DM message: {}", e);
                return Err(HandlerError::parse_failed(format!("storage error: {}", e)));
            }
        };

        tracing::debug!(
            "Received and stored anonymous DM message {} from {} in conversation {}",
            message_id,
            sender_id,
            conversation_id
        );

        // Emit event for UI with the stored message ID and sent_at for ordering
        ctx.emit(DomainEvent::AnonDmMessageReceived {
            conversation_id,
            sender_id,
            message_id,
            inner_content: inner_content.clone(),
            sent_at,
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an anonymous call offer.
    fn handle_call_offer(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        offer: &content_v1::AnonCallOffer,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        tracing::info!(
            conversation_id = %conversation_id,
            sender_id = %sender_id,
            room_name = %offer.room_name,
            call_type = ?offer.call_type,
            "Received anonymous call offer"
        );

        // Emit event for UI to show incoming call
        ctx.emit(DomainEvent::AnonCallOfferReceived {
            conversation_id,
            sender_id,
            call_room_id: offer.call_room_id.clone(),
            room_name: offer.room_name.clone(),
            call_type: offer.call_type,
            caller_display_name: offer.caller_display_name.clone(),
            call_encryption_key: offer.call_encryption_key.clone(),
            timestamp: offer.timestamp,
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an anonymous call answer.
    fn handle_call_answer(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        answer: &content_v1::AnonCallAnswer,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        tracing::info!(
            conversation_id = %conversation_id,
            sender_id = %sender_id,
            accepted = answer.accepted,
            "Received anonymous call answer"
        );

        // Emit event for UI to handle answer
        ctx.emit(DomainEvent::AnonCallAnswerReceived {
            conversation_id,
            sender_id,
            call_room_id: answer.call_room_id.clone(),
            accepted: answer.accepted,
            callee_display_name: answer.callee_display_name.clone(),
            timestamp: answer.timestamp,
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an anonymous call hangup.
    fn handle_call_hangup(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        hangup: &content_v1::AnonCallHangup,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        tracing::info!(
            conversation_id = %conversation_id,
            sender_id = %sender_id,
            reason = ?hangup.reason,
            "Received anonymous call hangup"
        );

        // Emit event for UI to end call
        ctx.emit(DomainEvent::AnonCallHangupReceived {
            conversation_id,
            sender_id,
            call_room_id: hangup.call_room_id.clone(),
            reason: hangup.reason,
            timestamp: hangup.timestamp,
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Convert Content proto to storage format (content_type, content_json).
    fn content_to_storage(content: &Content) -> (i32, String) {
        match &content.content {
            Some(content_v1::content::Content::DataMessage(data_msg)) => {
                // Text message
                (1, serde_json::json!({ "text": data_msg.body }).to_string())
            }
            Some(content_v1::content::Content::Attachment(att)) => {
                // Attachment message
                (2, serde_json::json!({
                    "caption": att.caption,
                    "channel_id": att.channel_id,
                }).to_string())
            }
            Some(content_v1::content::Content::Reaction(reaction)) => {
                (3, serde_json::json!({
                    "emoji": reaction.emoji,
                    "target_sent_timestamp": reaction.target_sent_timestamp,
                    "remove": reaction.remove,
                }).to_string())
            }
            _ => {
                // Unknown content type - store as empty JSON
                (0, "{}".to_string())
            }
        }
    }
}

#[async_trait::async_trait]
impl InboxHandler for AnonDmHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Get sender ID
        let sender_id = self.get_sender_id(message)?;

        // Route based on content type
        // Note: All methods are sync (no encryption) to satisfy Send bounds
        match content.content {
            Some(content_v1::content::Content::AnonDmRequest(request)) => {
                self.handle_request(request, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonDmAccept(accept)) => {
                self.handle_accept(accept, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonDmMessage(msg)) => {
                self.handle_message(msg, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonDmPresence(presence)) => {
                self.handle_presence(presence, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonDmProfileUpdate(profile_update)) => {
                self.handle_profile_update(profile_update, sender_id, ctx)
            }
            _ => Ok(HandlerOutcome::Ignored),
        }
    }

    fn content_types(&self) -> &[ContentType] {
        // We handle DataMessage since anon DM messages come through sealed sender
        &[ContentType::DataMessage]
    }
}
