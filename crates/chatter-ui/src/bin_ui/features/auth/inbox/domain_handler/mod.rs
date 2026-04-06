//! Domain event handler for UI updates.
//!
//! This is the single entry point for all UI updates from incoming events.
//! Protocol concerns (receipts, convergence, sender keys) are handled by
//! core's `InboxEventProcessor` before events reach here.
//!
//! ```text
//! Polling → InboxEventProcessor (core) → DomainEvent → domain_handler (this module)
//! ```

mod anon;
mod anon_calls;
mod calls;
mod helpers;
pub(crate) use helpers::recompute_total_unread;
mod messaging;
mod social;

use std::sync::Arc;

use slint::Weak;

use chatter_core::domain::ids::MessageId;
use chatter_core::domain::message::{ConversationType, Message, MessageContent, SystemMessage};
use chrono::Utc;
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
        // Call Key Answer + Group Call Key
        // Handled by calls/event_handlers.rs (wire_call_key_answer_handler)
        // which delegates to key_exchange for ECDH and call_service for group keys.
        // =========================================================================
        DomainEvent::CallKeyAnswerReceived { .. }
        | DomainEvent::GroupCallKeyReceived { .. } => {}

        // =========================================================================
        // Call Hangup
        // =========================================================================
        DomainEvent::CallHangupReceived { call_id, peer_id, reason } => {
            calls::handle_call_hangup(window, ui_ctx, call_id, peer_id, reason);
        }

        // =========================================================================
        // Typing Indicators (pass-through / no-op)
        // =========================================================================
        DomainEvent::TypingStarted { user_id: _, channel_id } => {
            super::unified_actions::handle_typing(
                window,
                ui_ctx.chat_state(),
                super::unified_actions::EventSource::Channel(channel_id),
                true,
            );
        }

        DomainEvent::TypingStopped { user_id: _, channel_id } => {
            super::unified_actions::handle_typing(
                window,
                ui_ctx.chat_state(),
                super::unified_actions::EventSource::Channel(channel_id),
                false,
            );
        }

        // =========================================================================
        // Message Events (handled by EventHandlers state store)
        // =========================================================================
        DomainEvent::MessageReceived { ref message } => {
            messaging::handle_message_received(window, ui_ctx, message.clone());
        }

        // Receipt events: update checkmarks (✓✓) in the Slint message model.
        DomainEvent::MessagesDelivered { message_timestamps, .. } => {
            messaging::handle_delivery_status_update(window, message_timestamps, "delivered");
        }

        DomainEvent::MessagesRead { message_timestamps, .. } => {
            messaging::handle_delivery_status_update(window, message_timestamps, "read");
        }

        // Reaction UI updates: update emoji summary on the message bubble + sidebar
        // Uses Signal-style (sender_id, sent_at_ns) composite key for matching.
        DomainEvent::ReactionAdded { sender_id, sent_at_ns, emoji, .. } => {
            messaging::handle_reaction_update(window, &sender_id.to_string(), sent_at_ns, &emoji, false);
            messaging::refresh_sidebar_for_reaction(window, ui_ctx, &emoji, false);
        }
        DomainEvent::ReactionRemoved { sender_id, sent_at_ns, emoji, .. } => {
            messaging::handle_reaction_update(window, &sender_id.to_string(), sent_at_ns, &emoji, true);
            messaging::refresh_sidebar_for_reaction(window, ui_ctx, &emoji, true);
        }

        // Edit UI update: replace message content in-place
        DomainEvent::MessageEdited { message_id, new_content, target_sent_timestamp, .. } => {
            use chatter_core::domain::message::MessageContent;
            let new_text = match &new_content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
                MessageContent::AI(ai) => ai.text.clone(),
                _ => String::new(),
            };
            // Timestamp-primary matching (Signal-style). UUID fallback for legacy.
            messaging::handle_message_edit(
                window, &message_id.to_string(), target_sent_timestamp, &new_text,
            );
        }

        // Delete UI update: remove message from visible list (timestamp-primary matching)
        DomainEvent::MessageDeleted { message_id, target_sent_timestamp, .. } => {
            messaging::handle_message_delete(window, &message_id.to_string(), target_sent_timestamp);
        }

        // State store handles these (no Slint model update needed)
        DomainEvent::ViewedReceipt { .. }
        | DomainEvent::DeleteForMeSync { .. } => {}

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
        | DomainEvent::GroupMailboxReceived { .. } => {
            // Protocol events - no direct UI action
        }

        // Attachment ready: trigger download + UI update for the receiver.
        // When the receiver gets a message with attachments, the attachment metadata
        // may not be available yet during MessageReceived handling. AttachmentReady
        // fires after the attachment pointer is processed and stored.
        DomainEvent::AttachmentReady { attachment, channel_id, .. } => {
            let downloadable = if attachment.remote_url.is_some() && attachment.encryption_key.is_some() {
                vec![attachment]
            } else {
                vec![]
            };
            if !downloadable.is_empty() {
                let att_svc = ui_ctx.attachments().clone();
                let weak = window.clone();
                let conv_id = channel_id.to_string();
                tokio::spawn(async move {
                    super::attachment_download::download_attachments(
                        &downloadable,
                        att_svc,
                        weak,
                        conv_id,
                    ).await;
                });
            }
        }

        // =========================================================================
        // Anonymous Group Events
        // =========================================================================
        DomainEvent::AnonGroupInviteReceived { group_id, group_name, inviter_id, .. } => {
            anon::handle_group_invite_received(window, ui_ctx, group_id, group_name, inviter_id);
        }

        DomainEvent::AnonGroupJoined { group_id } => {
            anon::handle_group_joined(window, ui_ctx, group_id);
        }

        DomainEvent::AnonGroupMemberJoined { group_id, user_id, .. } => {
            let now = Utc::now();
            let message = Message {
                id: MessageId::new(),
                conversation_id: group_id.into(),
                conversation_type: ConversationType::AnonGroup,
                sender_id: Some(user_id),
                device_id: None,
                content: MessageContent::System(SystemMessage::UserJoined(user_id)),
                encrypted_content: None,
                thread_id: None,
                reply_to: None,
                reply_to_author_id: None,
                reply_to_text: None,
                attachments: Vec::new(),
                is_pinned: false,
                is_edited: false,
                is_deleted: false,
                is_read: true,
                sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
                created_at: now,
                updated_at: now,
                edited_at: None,
                reaction_counts: Vec::new(),
                current_user_reactions: Vec::new(),
                mentioned_integrations: Vec::new(),
                targets_ai_bot: false,
                delivery_status: String::new(),
            };
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, group_id.into());
        }

        DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
            let now = Utc::now();
            let message = Message {
                id: MessageId::new(),
                conversation_id: group_id.into(),
                conversation_type: ConversationType::AnonGroup,
                sender_id: Some(user_id),
                device_id: None,
                content: MessageContent::System(SystemMessage::UserLeft(user_id)),
                encrypted_content: None,
                thread_id: None,
                reply_to: None,
                reply_to_author_id: None,
                reply_to_text: None,
                attachments: Vec::new(),
                is_pinned: false,
                is_edited: false,
                is_deleted: false,
                is_read: true,
                sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
                created_at: now,
                updated_at: now,
                edited_at: None,
                reaction_counts: Vec::new(),
                current_user_reactions: Vec::new(),
                mentioned_integrations: Vec::new(),
                targets_ai_bot: false,
                delivery_status: String::new(),
            };
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, group_id.into());
        }

        DomainEvent::AnonGroupMemberKicked { group_id, user_id, kicked_by } => {
            let now = Utc::now();
            let message = Message {
                id: MessageId::new(),
                conversation_id: group_id.into(),
                conversation_type: ConversationType::AnonGroup,
                sender_id: Some(kicked_by), // Admin who did it
                device_id: None,
                content: MessageContent::System(SystemMessage::UserRemoved { by: kicked_by, removed: user_id }),
                encrypted_content: None,
                thread_id: None,
                reply_to: None,
                reply_to_author_id: None,
                reply_to_text: None,
                attachments: Vec::new(),
                is_pinned: false,
                is_edited: false,
                is_deleted: false,
                is_read: true,
                sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
                created_at: now,
                updated_at: now,
                edited_at: None,
                reaction_counts: Vec::new(),
                current_user_reactions: Vec::new(),
                mentioned_integrations: Vec::new(),
                targets_ai_bot: false,
                delivery_status: String::new(),
            };
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, group_id.into());
        }

        DomainEvent::AnonGroupBootstrapComplete { group_id } => {
            anon::handle_group_bootstrap_complete(group_id);
        }

        DomainEvent::AnonGroupMemberListReceived { group_id, ref members } => {
            anon::handle_group_member_list_received(window, ui_ctx, group_id, members.len());
        }

        DomainEvent::AnonGroupUpdated { group_id, updated_by, .. } => {
            anon::handle_group_updated(window, ui_ctx, group_id, updated_by);
        }

        // =========================================================================
        // Anonymous DM Events
        // =========================================================================
        DomainEvent::AnonDmRequestReceived { .. } => {
            anon::handle_dm_request_received(window, ui_ctx);
        }

        DomainEvent::AnonDmRequestAccepted { conversation_id, peer_id, .. } => {
            anon::handle_dm_request_accepted(window, ui_ctx, conversation_id, peer_id);
        }

        DomainEvent::AnonDmInitReceived { .. } => {
            anon::handle_dm_init_received(window, ui_ctx);
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
            messaging::handle_message_send_failed(window, channel_id, error);
        }

        // =========================================================================
        // Channel Member Events
        // =========================================================================
        DomainEvent::ChannelMemberJoined { channel_id, user_id: _, display_name: _, message } => {
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, channel_id);
        }

        DomainEvent::ChannelMemberLeft { channel_id, user_id: _, display_name: _, message } => {
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, channel_id);
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

        // Handled by EventHandlers state store (appends to message_state)
        DomainEvent::SavedMessageSynced { .. } => {}

        // Jarvis AI events — UI can subscribe directly for display
        DomainEvent::JarvisResponse { .. }
        | DomainEvent::JarvisStats { .. }
        | DomainEvent::JarvisCleared { .. } => {}
    }
}
