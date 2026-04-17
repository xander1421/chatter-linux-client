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
use crate::domain::message::ConversationType;
use crate::domain::UserId;
use crate::inbox::processor::{ContentType, ProcessedMessage};
use crate::proto::content_v1::{self, Content};
use crate::services::anon_group::AnonGroupService;

use super::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome, HandlerResult, InboxHandler};
use super::base_anon::{self, AnonContentService};

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

}

/// Bridge between the shared `AnonContentService` trait and `AnonGroupService`.
struct GroupContentBridge<'a>(&'a AnonGroupService);

impl AnonContentService for GroupContentBridge<'_> {
    type Id = AnonGroupId;

    fn delete_message_by_timestamp(&self, id: &AnonGroupId, sender_id: &UserId, ts: u64) -> Result<bool, String> {
        self.0.delete_message_by_timestamp(id, sender_id, ts).map_err(|e| e.to_string())
    }

    fn update_message_text_by_sender(&self, id: &AnonGroupId, sender_id: &UserId, ts: u64, text: &str) -> Result<bool, String> {
        self.0.update_message_text_by_sender(id, sender_id, ts, text).map_err(|e| e.to_string())
    }

    fn store_message(
        &self,
        id: &AnonGroupId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
        quote: Option<super::base_anon::QuoteContext>,
    ) -> Result<String, String> {
        self.0.store_message(id, sender_id, content_type, content_json, sent_at, quote)
            .map(|id| id.as_str().to_string())
            .map_err(|e| e.to_string())
    }

    fn add_reaction(
        &self,
        id: &AnonGroupId,
        target_sender_id: &UserId,
        target_sent_at_ns: u64,
        reactor_id: &UserId,
        emoji: &str,
    ) -> Result<bool, String> {
        self.0
            .add_reaction(id, target_sender_id, target_sent_at_ns, reactor_id, emoji)
            .map_err(|e| e.to_string())
    }

    fn remove_reaction(
        &self,
        id: &AnonGroupId,
        target_sender_id: &UserId,
        target_sent_at_ns: u64,
        reactor_id: &UserId,
        emoji: &str,
    ) -> Result<bool, String> {
        self.0
            .remove_reaction(id, target_sender_id, target_sent_at_ns, reactor_id, emoji)
            .map_err(|e| e.to_string())
    }

    fn conversation_type(&self) -> ConversationType { ConversationType::AnonGroup }

    fn label(&self) -> &'static str { "anon group" }
}

impl AnonGroupHandler {
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

        use crate::domain::message::SystemMessage;
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let _ = self.service.store_system_message(
            &group_id,
            Some(sender_id),
            SystemMessage::UserJoined(sender_id),
            sent_at,
        );

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

        use crate::domain::message::SystemMessage;
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let _ = self.service.store_system_message(
            &group_id,
            Some(sender_id),
            SystemMessage::UserLeft(sender_id),
            sent_at,
        );

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

        use crate::domain::message::SystemMessage;
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let _ = self.service.store_system_message(
            &group_id,
            Some(sender_id),
            SystemMessage::UserRemoved { by: sender_id, removed: kicked_user_id },
            sent_at,
        );

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
    fn handle_typing(&self, id: AnonGroupId, sender_id: UserId, typing: &content_v1::TypingMessage, ctx: &HandlerContext) -> HandlerResult {
        base_anon::handle_typing(id, sender_id, typing, ctx, &GroupContentBridge(&self.service))
    }

    /// Handle a reaction.
    fn handle_reaction(&self, id: AnonGroupId, sender_id: UserId, reaction: &content_v1::Reaction, ctx: &HandlerContext) -> HandlerResult {
        base_anon::handle_reaction(id, sender_id, reaction, ctx, &GroupContentBridge(&self.service))
    }

    /// Handle a read receipt.
    fn handle_receipt(&self, id: AnonGroupId, sender_id: UserId, receipt: &content_v1::ReceiptMessage, ctx: &HandlerContext) -> HandlerResult {
        base_anon::handle_receipt(id, sender_id, receipt, ctx, &GroupContentBridge(&self.service))
    }

    /// Handle a delete-for-everyone message.
    fn handle_delete(&self, id: AnonGroupId, sender_id: UserId, target_sent_timestamp: u64, ctx: &HandlerContext) -> HandlerResult {
        base_anon::handle_delete(id, sender_id, target_sent_timestamp, ctx, &GroupContentBridge(&self.service))
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

        let pinned = !pin.unpin;

        // Persist pin state to database
        if let Err(e) = self.service.set_message_pinned(&group_id, pin.target_sent_timestamp, pinned) {
            tracing::error!(
                error = %e,
                group_id = %group_id,
                timestamp = pin.target_sent_timestamp,
                "Failed to persist anon group pin state"
            );
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
    fn handle_edit(&self, id: AnonGroupId, sender_id: UserId, edit: &content_v1::EditMessage, ctx: &HandlerContext) -> HandlerResult {
        base_anon::handle_edit(id, sender_id, edit, ctx, &GroupContentBridge(&self.service))
    }

    /// Handle a regular data message (text, etc.).
    fn handle_data_message(&self, id: AnonGroupId, sender_id: UserId, inner_content: &Content, ctx: &HandlerContext) -> HandlerResult {
        base_anon::handle_data_message(id, sender_id, inner_content, ctx, &GroupContentBridge(&self.service))
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
