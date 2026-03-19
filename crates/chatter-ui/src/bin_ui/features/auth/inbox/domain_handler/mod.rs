//! Domain event handler for UI updates.
//!
//! This module handles `DomainEvent` for pure UI updates. Protocol concerns
//! (receipts, convergence, sender keys) are handled by `InboxEventProcessor`.
//!
//! # Architecture
//!
//! ```text
//! InboxEvent → InboxEventProcessor → DomainEvent → domain_handler (this module)
//!                 (protocol)                           (UI only)
//! ```

mod anon_calls;
mod anon_dm;
mod anon_group;
mod calls;
mod helpers;
mod messaging;
mod social;

use std::sync::Arc;

use slint::Weak;

use chatter_core::inbox::handlers::DomainEvent;
use chatter_core::client::ChatterClient;
use crate::MainWindow;

/// Handle a domain event for UI updates only.
///
/// This function should contain NO protocol logic - only UI updates.
/// Protocol concerns are handled by `InboxEventProcessor` before events reach here.
pub async fn handle_domain_event(
    event: DomainEvent,
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    match event {
        // =========================================================================
        // Unread Count Updates
        // =========================================================================
        DomainEvent::UnreadCountUpdated {
            channel_id,
            peer_id,
            unread_count,
        } => {
            messaging::handle_unread_count_updated(window, channel_id, peer_id, unread_count);
        }

        // =========================================================================
        // DM Bootstrap Completed
        // =========================================================================
        DomainEvent::DmBootstrapCompleted { peer_id } => {
            messaging::handle_dm_bootstrap_completed(window, ui_ctx, peer_id);
        }

        // =========================================================================
        // Friend Request Updates
        // =========================================================================
        DomainEvent::FriendRequestsUpdated { received, sent } => {
            social::handle_friend_requests_updated(window, received, sent);
        }

        // =========================================================================
        // Friend Accepted
        // =========================================================================
        DomainEvent::FriendAccepted { friend_id, channel_id } => {
            social::handle_friend_accepted(window, ui_ctx, friend_id, channel_id);
        }

        // =========================================================================
        // Call Key Offer (Incoming Call)
        // =========================================================================
        DomainEvent::CallKeyOfferReceived {
            call_id,
            peer_id,
            ephemeral_public_key,
            call_type,
            channel_id,
            is_identity_verified,
            identity_warning,
            caller_name,
            timestamp,
        } => {
            calls::handle_call_key_offer(
                window, ui_ctx, call_id, peer_id, ephemeral_public_key,
                call_type, channel_id, is_identity_verified, identity_warning,
                caller_name, timestamp,
            );
        }

        // =========================================================================
        // Call Key Answer
        // =========================================================================
        DomainEvent::CallKeyAnswerReceived {
            call_id,
            peer_id,
            ephemeral_public_key,
        } => {
            calls::handle_call_key_answer(window, ui_ctx, call_id, peer_id, ephemeral_public_key);
        }

        // =========================================================================
        // Call Hangup
        // =========================================================================
        DomainEvent::CallHangupReceived { call_id, peer_id, reason } => {
            calls::handle_call_hangup(window, ui_ctx, call_id, peer_id, reason);
        }

        // =========================================================================
        // Group Call Key
        // =========================================================================
        DomainEvent::GroupCallKeyReceived {
            call_id,
            sender_id,
            key_material,
            key_epoch,
            channel_id,
        } => {
            calls::handle_group_call_key(ui_ctx, call_id, sender_id, key_material, key_epoch, channel_id);
        }

        // =========================================================================
        // Typing Indicators (pass-through / no-op)
        // =========================================================================
        DomainEvent::TypingStarted { user_id, channel_id } => {
            tracing::debug!(
                user_id = %user_id,
                channel_id = %channel_id,
                "Typing started event received"
            );
        }

        DomainEvent::TypingStopped { user_id, channel_id } => {
            tracing::debug!(
                user_id = %user_id,
                channel_id = %channel_id,
                "Typing stopped event received"
            );
        }

        // =========================================================================
        // Message Events (handled by EventHandlers state store)
        // =========================================================================
        DomainEvent::MessageReceived { .. }
        | DomainEvent::MessageEdited { .. }
        | DomainEvent::MessageDeleted { .. }
        | DomainEvent::MessagesRead { .. }
        | DomainEvent::MessagesDelivered { .. }
        | DomainEvent::ViewedReceipt { .. }
        | DomainEvent::ReactionAdded { .. }
        | DomainEvent::ReactionRemoved { .. }
        | DomainEvent::DeleteForMeSync { .. } => {
            // These are handled by EventHandlers in events/handlers.rs
            // which updates state stores directly
        }

        DomainEvent::MessagePinned { channel_id, target_sent_timestamp, .. } => {
            helpers::handle_pin_ui_update(window, ui_ctx, channel_id, target_sent_timestamp, false);
        }

        DomainEvent::MessageUnpinned { channel_id, target_sent_timestamp, .. } => {
            helpers::handle_pin_ui_update(window, ui_ctx, channel_id, target_sent_timestamp, true);
        }

        // =========================================================================
        // Protocol Events (handled by InboxEventProcessor, no UI action needed)
        // =========================================================================
        DomainEvent::KeyExchangeReceived { .. }
        | DomainEvent::SenderKeyReceived { .. }
        | DomainEvent::GroupMailboxReceived { .. }
        | DomainEvent::AttachmentReady { .. } => {
            // Protocol events - no direct UI action
        }

        // =========================================================================
        // Anonymous Group Events
        // =========================================================================
        DomainEvent::AnonGroupInviteReceived { group_id, group_name, inviter_id, .. } => {
            anon_group::handle_invite_received(window, ui_ctx, group_id, group_name, inviter_id);
        }

        DomainEvent::AnonGroupJoined { group_id } => {
            anon_group::handle_joined(window, ui_ctx, group_id);
        }

        DomainEvent::AnonGroupMemberJoined { group_id, user_id, display_name, .. } => {
            anon_group::handle_member_joined(group_id, user_id, display_name);
        }

        DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
            anon_group::handle_member_left(group_id, user_id);
        }

        DomainEvent::AnonGroupMemberKicked { group_id, user_id, kicked_by } => {
            anon_group::handle_member_kicked(group_id, user_id, kicked_by);
        }

        DomainEvent::AnonGroupMessageReceived { group_id, sender_id, message_id, .. } => {
            anon_group::handle_message_received(window, ui_ctx, group_id, sender_id, message_id);
        }

        DomainEvent::AnonGroupBootstrapComplete { group_id } => {
            anon_group::handle_bootstrap_complete(group_id);
        }

        DomainEvent::AnonGroupMemberListReceived { group_id, members } => {
            anon_group::handle_member_list_received(window, ui_ctx, group_id, members);
        }

        DomainEvent::AnonGroupUpdated { group_id, updated_by, .. } => {
            anon_group::handle_updated(window, ui_ctx, group_id, updated_by);
        }

        // =========================================================================
        // Anonymous DM Events
        // =========================================================================
        DomainEvent::AnonDmRequestReceived { .. } => {
            anon_dm::handle_request_received(window, ui_ctx);
        }

        DomainEvent::AnonDmRequestAccepted { conversation_id, peer_id, .. } => {
            anon_dm::handle_request_accepted(window, ui_ctx, conversation_id, peer_id);
        }

        DomainEvent::AnonDmMessageReceived { .. } => {
            anon_dm::handle_message_received(window, ui_ctx);
        }

        DomainEvent::AnonDmInitReceived { .. } => {
            anon_dm::handle_init_received(window, ui_ctx);
        }

        DomainEvent::AnonDmMessageDeleted { .. }
        | DomainEvent::AnonDmMessageEdited { .. }
        | DomainEvent::AnonGroupMessageEdited { .. } => {
            // Handled by message state
        }

        // =========================================================================
        // Anonymous Call Events
        // =========================================================================
        DomainEvent::AnonCallOfferReceived {
            conversation_id,
            sender_id,
            call_room_id,
            room_name,
            call_type,
            caller_display_name,
            call_encryption_key,
            ..
        } => {
            anon_calls::handle_anon_call_offer(
                window, ui_ctx, conversation_id, sender_id, call_room_id,
                room_name, call_type, caller_display_name, call_encryption_key,
            );
        }

        DomainEvent::AnonCallAnswerReceived {
            conversation_id,
            accepted,
            call_room_id,
            callee_display_name,
            ..
        } => {
            anon_calls::handle_anon_call_answer(
                window, ui_ctx, conversation_id, accepted, call_room_id, callee_display_name,
            );
        }

        DomainEvent::AnonCallHangupReceived { conversation_id, .. } => {
            anon_calls::handle_anon_call_hangup(window, conversation_id);
        }

        // =========================================================================
        // Anonymous Group Call Events
        // =========================================================================
        DomainEvent::AnonGroupCallOfferReceived {
            group_id,
            sender_id,
            call_room_id,
            room_name,
            call_type,
            caller_display_name,
            call_encryption_key,
            ..
        } => {
            anon_calls::handle_anon_group_call_offer(
                window, ui_ctx, group_id, sender_id, call_room_id,
                room_name, call_type, caller_display_name, call_encryption_key,
            );
        }

        DomainEvent::AnonGroupCallAnswerReceived {
            group_id,
            accepted,
            call_room_id,
            callee_display_name,
            ..
        } => {
            anon_calls::handle_anon_group_call_answer(
                window, ui_ctx, group_id, accepted, call_room_id, callee_display_name,
            );
        }

        DomainEvent::AnonGroupCallHangupReceived { group_id, .. } => {
            anon_calls::handle_anon_group_call_hangup(window, group_id);
        }

        // =========================================================================
        // Message Send Events
        // =========================================================================
        DomainEvent::MessageSent { .. } => {
            messaging::handle_message_sent();
        }

        DomainEvent::MessageSendFailed { channel_id, error } => {
            messaging::handle_message_send_failed(channel_id, error);
        }

        // =========================================================================
        // Channel Member Events
        // =========================================================================
        DomainEvent::ChannelMemberJoined { channel_id, user_id, display_name, message: _ } => {
            messaging::handle_channel_member_joined(channel_id, user_id, display_name);
        }

        DomainEvent::ChannelMemberLeft { channel_id, user_id, display_name, message: _ } => {
            messaging::handle_channel_member_left(channel_id, user_id, display_name);
        }

        // =========================================================================
        // User Presence Updates
        // =========================================================================
        DomainEvent::UserPresenceChanged {
            user_id,
            presence,
            timestamp,
        } => {
            social::handle_presence_changed(window, user_id, presence, timestamp);
        }

        // =========================================================================
        // Anonymous DM Profile Updates
        // =========================================================================
        DomainEvent::AnonDmProfileUpdated {
            conversation_id,
            user_id,
            display_name,
            avatar_url,
            bio,
            timestamp,
        } => {
            social::handle_anon_dm_profile_updated(
                window, ui_ctx, conversation_id, user_id, display_name, avatar_url, bio, timestamp,
            );
        }
    }
}
