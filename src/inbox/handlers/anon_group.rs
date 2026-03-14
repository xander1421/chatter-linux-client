//! Anonymous group message handler.
//!
//! Handles incoming anonymous group messages: invites, join acks, member lists,
//! leave notifications, kick notifications, and group messages.
//!
//! Note: This handler only parses messages and emits events. Any encrypted
//! responses (like broadcasting member lists after join acks) are handled
//! by the UI layer in response to events.

use std::str::FromStr;
use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::ChannelId;
use crate::domain::UserId;
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{self, typing_message, receipt_message, Content};
use crate::services::anon_group::AnonGroupService;

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};

// ============================================================================
// AnonGroupHandler
// ============================================================================

/// Handler for anonymous group messages.
///
/// Processes incoming group management messages (invites, acks, member lists)
/// and group content messages. Emits domain events for UI notification.
///
/// Note: This handler does NOT call async service methods that involve
/// encryption, as those are not Send-safe. Instead, it updates local state
/// synchronously and emits events. Any outgoing encrypted messages (like
/// broadcasting member lists) should be triggered by the UI layer.
pub struct AnonGroupHandler {
    service: Arc<AnonGroupService>,
}

impl AnonGroupHandler {
    /// Create a new anonymous group handler.
    pub fn new(service: Arc<AnonGroupService>) -> Self {
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

    /// Handle an incoming invite.
    fn handle_invite(
        &self,
        invite: content_v1::AnonGroupInvite,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&invite.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in invite"))?;

        let inviter_mailbox_pk = if invite.inviter_mailbox_pk.len() == 32 {
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&invite.inviter_mailbox_pk);
            pk
        } else {
            return Err(HandlerError::parse_failed("Invalid inviter mailbox public key"));
        };

        let profile_key = if let Some(ref pk) = invite.profile_key {
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

        use crate::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

        let inviter_display_name: Option<String> = match (&profile_key, &invite.encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => Some(name),
                    _ => None,
                }
            }
            _ => None,
        };

        let inviter_avatar: Option<Vec<u8>> = match (&profile_key, &invite.encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_avatar(pk, encrypted).ok()
            }
            _ => None,
        };

        // Emit event for UI to show invite dialog
        ctx.emit(DomainEvent::AnonGroupInviteReceived {
            group_id,
            group_name: invite.group_name.clone(),
            inviter_id: sender_id,
            inviter_mailbox_pk,
            profile_key,
            inviter_display_name,
            inviter_avatar,
        });

        tracing::info!(
            "Received invite to group {} ('{}') from {}",
            group_id,
            invite.group_name,
            sender_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming join acknowledgment.
    ///
    /// Updates local member state synchronously and emits event.
    /// The UI layer should call `service.broadcast_member_list()` in response
    /// if we are the group owner.
    async fn handle_join_ack(
        &self,
        ack: content_v1::AnonGroupJoinAck,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&ack.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in join ack"))?;

        let joiner_mailbox_pk = if ack.joiner_mailbox_pk.len() == 32 {
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&ack.joiner_mailbox_pk);
            pk
        } else {
            return Err(HandlerError::parse_failed("Invalid joiner mailbox public key"));
        };

        let profile_key = if let Some(ref pk) = ack.profile_key {
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

        use crate::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

        let display_name: Option<String> = match (&profile_key, &ack.encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => Some(name),
                    _ => None,
                }
            }
            _ => None,
        };

        let avatar: Option<Vec<u8>> = match (&profile_key, &ack.encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_avatar(pk, encrypted).ok()
            }
            _ => None,
        };

        // Process the join ack via service (async - handles broadcast)
        if let Err(e) = self
            .service
            .handle_join_ack(
                group_id,
                sender_id,
                joiner_mailbox_pk,
                profile_key,
                display_name.clone(),
                avatar.clone(),
            )
            .await
        {
            tracing::warn!("Failed to handle join ack for {}: {}", sender_id, e);
        }

        // Emit event for UI
        ctx.emit(DomainEvent::AnonGroupMemberJoined {
            group_id,
            user_id: sender_id,
            profile_key,
            display_name,
            avatar,
        });

        tracing::info!("Member {} joined group {}", sender_id, group_id);

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming member list update.
    ///
    /// Updates local member state and emits event. The UI layer should trigger
    /// profile sharing with the new members we learned about.
    ///
    /// SECURITY: Only accepts member lists from admins/owners to prevent
    /// regular members from spoofing other members' profiles.
    fn handle_member_list(
        &self,
        list: content_v1::AnonGroupMemberList,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&list.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in member list"))?;

        // SECURITY: Verify sender is admin or owner before accepting member list
        // This prevents regular members from spoofing other members' profiles
        match self.service.get_member_role(&group_id, &sender_id) {
            Ok(Some(role)) if role.can_broadcast_member_list() => {
                // Sender is authorized to broadcast member lists
            }
            Ok(Some(role)) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    role = ?role,
                    "Rejected member list from non-admin member"
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Ok(None) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Rejected member list from non-member"
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Err(e) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    error = %e,
                    "Failed to verify sender role for member list"
                );
                return Ok(HandlerOutcome::Ignored);
            }
        }

        // SECURITY: Verify sender is admin or owner before accepting member list
        // This prevents regular members from spoofing other members' profiles
        match self.service.get_member_role(&group_id, &sender_id) {
            Ok(Some(role)) if role.can_broadcast_member_list() => {
                // Sender is authorized to broadcast member lists
            }
            Ok(Some(role)) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    ?role,
                    "Rejected member list from unauthorized member"
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Ok(None) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Rejected member list from non-member"
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Err(e) => {
                tracing::error!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    error = %e,
                    "Failed to verify sender role for member list"
                );
                return Ok(HandlerOutcome::Ignored);
            }
        }

        // Process the member list
        if let Err(e) = self.service.handle_member_list(group_id, sender_id, list.members.clone()) {
            tracing::warn!("Failed to handle member list for group {}: {}", group_id, e);
        }

        // Emit event for UI - the UI layer should trigger profile sharing
        ctx.emit(DomainEvent::AnonGroupMemberListReceived {
            group_id,
            members: list.members,
        });

        tracing::debug!("Updated member list for group {}", group_id);

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming leave notification.
    fn handle_leave(
        &self,
        leave: content_v1::AnonGroupLeave,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&leave.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in leave"))?;

        // Process the leave
        if let Err(e) = self.service.handle_leave(group_id, sender_id) {
            tracing::warn!("Failed to handle leave from {}: {}", sender_id, e);
        }

        // Emit event for UI
        ctx.emit(DomainEvent::AnonGroupMemberLeft {
            group_id,
            user_id: sender_id,
        });

        tracing::info!("Member {} left group {}", sender_id, group_id);

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming kick notification.
    fn handle_kick(
        &self,
        kick: content_v1::AnonGroupKick,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&kick.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in kick"))?;

        let kicked_user_id = UserId::try_from_slice(&kick.kicked_user_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid kicked user ID"))?;

        // Process the kick with signature verification
        if let Err(e) = self.service.handle_kick(
            group_id,
            kicked_user_id,
            sender_id,
            &kick.admin_signature,
            kick.timestamp,
        ) {
            tracing::warn!("Failed to handle kick: {}", e);
        }

        // Emit event for UI
        ctx.emit(DomainEvent::AnonGroupMemberKicked {
            group_id,
            user_id: kicked_user_id,
            kicked_by: sender_id,
        });

        tracing::info!(
            "Member {} kicked from group {} by {}",
            kicked_user_id,
            group_id,
            sender_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming profile share.
    ///
    /// Updates the sender's profile in the local member list.
    async fn handle_profile_share(
        &self,
        profile: content_v1::AnonGroupProfileShare,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&profile.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in profile share"))?;

        // Verify sender is a member
        match self.service.is_member(&group_id, &sender_id) {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    "Dropped profile share from non-member {} in group {}",
                    sender_id,
                    group_id
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Err(e) => {
                tracing::warn!("Failed to check membership: {}", e);
                return Ok(HandlerOutcome::Ignored);
            }
        }

        let profile_key = if let Some(ref pk) = profile.profile_key {
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

        use crate::crypto::profile_crypto::{decrypt_avatar, decrypt_display_name};

        let display_name: Option<String> = match (&profile_key, &profile.encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => Some(name),
                    _ => None,
                }
            }
            _ => None,
        };

        let avatar: Option<Vec<u8>> = match (&profile_key, &profile.encrypted_avatar) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_avatar(pk, encrypted).ok()
            }
            _ => None,
        };

        // Update member's profile via service (async - handles broadcast if owner)
        if let Err(e) = self
            .service
            .handle_profile_share(
                group_id,
                sender_id,
                profile_key,
                display_name.clone(),
                avatar.clone(),
            )
            .await
        {
            tracing::warn!(
                "Failed to handle profile share for {} in group {}: {}",
                sender_id,
                group_id,
                e
            );
        }

        // Emit event for UI
        ctx.emit(DomainEvent::AnonGroupMemberJoined {
            group_id,
            user_id: sender_id,
            profile_key,
            display_name: display_name.clone(),
            avatar,
        });

        tracing::debug!(
            "Received profile share from {} in group {} (name: {:?})",
            sender_id,
            group_id,
            display_name
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an incoming group message.
    #[allow(clippy::boxed_local)] // Box comes from proto definition for large nested messages
    fn handle_group_message(
        &self,
        msg: Box<content_v1::AnonGroupMessage>,
        sender_id: UserId,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        let group_id = AnonGroupId::try_from_slice(&msg.group_id)
            .ok_or_else(|| HandlerError::parse_failed("Invalid group ID in message"))?;

        // Check if group is blocked
        if self.service.is_group_blocked(&group_id).unwrap_or(false) {
            tracing::debug!("Dropped message for blocked group {}", group_id);
            return Ok(HandlerOutcome::Ignored);
        }

        // Check if sender is blocked in this group
        if self.service.is_user_blocked_in_group(&group_id, &sender_id).unwrap_or(false) {
            tracing::debug!(
                "Dropped message from blocked user {} in group {}",
                sender_id,
                group_id
            );
            return Ok(HandlerOutcome::Ignored);
        }

        // Verify sender is a member
        match self.service.is_member(&group_id, &sender_id) {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    "Dropped message from non-member {} in group {}",
                    sender_id,
                    group_id
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Err(e) => {
                tracing::warn!("Failed to check membership: {}", e);
                return Ok(HandlerOutcome::Ignored);
            }
        }

        // Extract the inner content
        let inner_content = msg.inner_content.ok_or_else(|| {
            HandlerError::parse_failed("Group message missing inner content")
        })?;

        // Route based on inner content type
        match &inner_content.content {
            Some(content_v1::content::Content::Typing(typing)) => {
                self.handle_typing(group_id, sender_id, typing, ctx)
            }
            Some(content_v1::content::Content::Reaction(reaction)) => {
                self.handle_reaction(group_id, sender_id, reaction, ctx)
            }
            Some(content_v1::content::Content::Receipt(receipt)) => {
                self.handle_receipt(group_id, sender_id, receipt, ctx)
            }
            Some(content_v1::content::Content::Pin(pin)) => {
                self.handle_pin(group_id, sender_id, pin, ctx)
            }
            Some(content_v1::content::Content::Edit(edit)) => {
                self.handle_edit(group_id, sender_id, edit, ctx)
            }
            Some(content_v1::content::Content::DataMessage(data_msg)) => {
                // Check for Delete first
                if let Some(delete) = &data_msg.delete {
                    return self.handle_delete(group_id, sender_id, delete.target_sent_timestamp, ctx);
                }
                // Regular data message
                self.handle_data_message(group_id, sender_id, &inner_content, ctx)
            }
            _ => {
                // Unknown or other content types - treat as data message
                self.handle_data_message(group_id, sender_id, &inner_content, ctx)
            }
        }
    }

    /// Handle a typing indicator.
    fn handle_typing(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        typing: &content_v1::TypingMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Convert AnonGroupId to ChannelId for the event (reuse existing event types)
        let channel_id: ChannelId = group_id.into();

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
            "Received typing indicator from {} in anon group {}",
            sender_id,
            group_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a reaction.
    fn handle_reaction(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        reaction: &content_v1::Reaction,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        use std::str::FromStr;
        // For anon groups, we emit the standard reaction event
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
            "Received reaction {} from {} in anon group {}",
            reaction.emoji,
            sender_id,
            group_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a read receipt.
    fn handle_receipt(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        receipt: &content_v1::ReceiptMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        if receipt.r#type == receipt_message::Type::Read as i32 {
            let channel_id: ChannelId = group_id.into();
            ctx.emit(DomainEvent::MessagesRead {
                reader_id: sender_id,
                message_timestamps: receipt.timestamps.clone(),
                receipt_timestamp: receipt.timestamp,
                channel_id: Some(channel_id),
            });

            tracing::debug!(
                "Received read receipt for {} messages from {} in anon group {}",
                receipt.timestamps.len(),
                sender_id,
                group_id
            );
        } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
            ctx.emit(DomainEvent::MessagesDelivered {
                recipient_id: sender_id,
                message_timestamps: receipt.timestamps.clone(),
                receipt_timestamp: receipt.timestamp,
            });

            tracing::debug!(
                "Received delivery receipt for {} messages from {} in anon group {}",
                receipt.timestamps.len(),
                sender_id,
                group_id
            );
        }

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a delete-for-everyone message.
    fn handle_delete(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        target_sent_timestamp: u64,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Delete the message from our local store
        match self.service.delete_message_by_timestamp(&group_id, &sender_id, target_sent_timestamp) {
            Ok(true) => {
                tracing::info!(
                    "Deleted message at timestamp {} from {} in anon group {}",
                    target_sent_timestamp,
                    sender_id,
                    group_id
                );
            }
            Ok(false) => {
                tracing::debug!(
                    "Message at timestamp {} not found for deletion in anon group {}",
                    target_sent_timestamp,
                    group_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to delete message at timestamp {} in anon group {}: {}",
                    target_sent_timestamp,
                    group_id,
                    e
                );
            }
        }

        // Emit event for UI to update
        let message_id = crate::domain::MessageId::from_timestamp(target_sent_timestamp);
        ctx.emit(DomainEvent::MessageDeleted {
            message_id,
            author_id: sender_id,
            channel_id: Some(group_id.into()),
        });

        Ok(HandlerOutcome::Processed)
    }

    /// Handle a pin/unpin message.
    fn handle_pin(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        pin: &content_v1::PinMessage,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // SECURITY: Verify sender is admin or owner before pinning for group
        match self.service.get_member_role(&group_id, &sender_id) {
            Ok(Some(role)) if role.can_pin() => {
                // Sender is authorized to pin/unpin
            }
            Ok(Some(role)) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    ?role,
                    "Rejected pin/unpin from unauthorized member"
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Ok(None) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    "Rejected pin/unpin from non-member"
                );
                return Ok(HandlerOutcome::Ignored);
            }
            Err(e) => {
                tracing::error!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    error = %e,
                    "Failed to verify sender role for pin/unpin"
                );
                return Ok(HandlerOutcome::Ignored);
            }
        }

        let message_id = crate::domain::MessageId::from_timestamp(pin.target_sent_timestamp);
        let channel_id: ChannelId = group_id.into();

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
            "Received {} for message at timestamp {} in anon group {}",
            if pin.unpin { "unpin" } else { "pin" },
            pin.target_sent_timestamp,
            group_id
        );

        Ok(HandlerOutcome::Processed)
    }

    /// Handle an edit message.
    ///
    /// Only updates the message if sender_id matches the message author.
    /// This ensures users can only edit their own messages.
    fn handle_edit(
        &self,
        group_id: AnonGroupId,
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
        match self.service.update_message_text_by_sender(&group_id, &sender_id, target_timestamp, &new_text) {
            Ok(true) => {
                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    target_timestamp = target_timestamp,
                    "Updated message text (edit from peer)"
                );
                // Emit event for UI to update
                ctx.emit(DomainEvent::AnonGroupMessageEdited {
                    group_id,
                    message_timestamp: target_timestamp,
                    new_text,
                });
            }
            Ok(false) => {
                // Either message not found or sender is not the author (security check)
                tracing::debug!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    target_timestamp = target_timestamp,
                    "Edit ignored: message not found or sender not author"
                );
            }
            Err(e) => {
                tracing::warn!(
                    sender_id = %sender_id,
                    group_id = %group_id,
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
        group_id: AnonGroupId,
        sender_id: UserId,
        inner_content: &Content,
        ctx: &HandlerContext,
    ) -> HandlerResult {
        // Determine content type and JSON representation for storage
        let (content_type, content_json) = Self::content_to_storage(inner_content);

        // Use sender's timestamp for consistent ordering across devices
        let sent_at = inner_content.timestamp;

        // Store the message
        let message_id = match self.service.store_message(&group_id, &sender_id, content_type, &content_json, sent_at) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to store group message: {}", e);
                return Err(HandlerError::parse_failed(format!("storage error: {}", e)));
            }
        };

        tracing::debug!(
            "Received and stored group message {} from {} in group {}",
            message_id,
            sender_id,
            group_id
        );

        // Emit event for UI with the stored message ID and sent_at for ordering
        ctx.emit(DomainEvent::AnonGroupMessageReceived {
            group_id,
            sender_id,
            message_id,
            inner_content: inner_content.clone(),
            sent_at,
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
impl InboxHandler for AnonGroupHandler {
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult {
        // Decode Content proto from plaintext
        let content: Content = ProstMessage::decode(message.plaintext.as_slice())
            .map_err(|e| HandlerError::parse_failed(e.to_string()))?;

        // Get sender ID
        let sender_id = self.get_sender_id(message)?;

        // Route based on content type
        // Note: All methods are sync (no encryption) to satisfy Send bounds
        match content.content {
            Some(content_v1::content::Content::AnonGroupInvite(invite)) => {
                self.handle_invite(invite, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonGroupJoinAck(ack)) => {
                self.handle_join_ack(ack, sender_id, ctx).await
            }
            Some(content_v1::content::Content::AnonGroupMemberList(list)) => {
                self.handle_member_list(list, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonGroupLeave(leave)) => {
                self.handle_leave(leave, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonGroupKick(kick)) => {
                self.handle_kick(kick, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonGroupMessage(msg)) => {
                self.handle_group_message(msg, sender_id, ctx)
            }
            Some(content_v1::content::Content::AnonGroupProfileShare(profile)) => {
                self.handle_profile_share(profile, sender_id, ctx).await
            }
            _ => Ok(HandlerOutcome::Ignored),
        }
    }

    fn content_types(&self) -> &[ContentType] {
        // We handle DataMessage since anon group messages come through sealed sender
        &[ContentType::DataMessage]
    }
}
