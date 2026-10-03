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
#[cfg(feature = "call-ui")]
mod anon_calls;
#[cfg(feature = "call-ui")]
mod calls;
mod helpers;
pub(crate) use helpers::{recompute_total_unread, update_unread_count};
mod messaging;
pub(crate) use messaging::set_own_status_at;
mod sessions;
mod social;

use std::sync::Arc;

use slint::Weak;

use chatter_core::events::DomainEvent;
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
        // DM Bootstrap Completed
        // =========================================================================
        DomainEvent::DmBootstrapCompleted { peer_id, sent_timestamps } => {
            messaging::handle_dm_bootstrap_completed(window, ui_ctx, peer_id, sent_timestamps);
        }

        // =========================================================================
        // Friend Request Updates
        // =========================================================================
        DomainEvent::FriendRequestsUpdated { received, sent } => {
            social::handle_friend_requests_updated(window, ui_ctx, received, sent).await;
        }

        // =========================================================================
        // Friend Accepted
        // =========================================================================
        DomainEvent::FriendAccepted { friend_id } => {
            social::handle_friend_accepted(window, ui_ctx, friend_id);
            // Friend just became mutual → Friends-tab list grows.
            crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                window.clone(),
                ui_ctx.clone(),
            );
        }

        // =========================================================================
        // Call Key Offer (Incoming Call)
        // =========================================================================
        #[cfg(feature = "call-ui")]
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
        #[cfg(not(feature = "call-ui"))]
        DomainEvent::CallKeyOfferReceived { .. } => {
            tracing::debug!("Ignoring CallKeyOfferReceived (desktop-media disabled)");
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
        #[cfg(feature = "call-ui")]
        DomainEvent::CallHangupReceived { call_id, peer_id, reason } => {
            calls::handle_call_hangup(window, ui_ctx, call_id, peer_id, reason);
        }
        #[cfg(not(feature = "call-ui"))]
        DomainEvent::CallHangupReceived { .. } => {
            tracing::debug!("Ignoring CallHangupReceived (desktop-media disabled)");
        }

        // =========================================================================
        // Typing Indicators (pass-through / no-op)
        // =========================================================================
        DomainEvent::TypingStarted { user_id, channel_id, timestamp } => {
            super::unified_actions::handle_typing(
                window,
                ui_ctx,
                super::unified_actions::EventSource::Channel(channel_id),
                user_id,
                timestamp,
                true,
            );
        }

        DomainEvent::TypingStopped { user_id, channel_id, timestamp } => {
            super::unified_actions::handle_typing(
                window,
                ui_ctx,
                super::unified_actions::EventSource::Channel(channel_id),
                user_id,
                timestamp,
                false,
            );
        }

        // =========================================================================
        // Message Events (handled by EventHandlers state store)
        // =========================================================================
        DomainEvent::MessageReceived { ref message } => {
            let is_server_dm = matches!(
                message.conversation_type,
                chatter_core::domain::message::ConversationType::ServerDm
            );
            messaging::handle_message_received(window, ui_ctx, message.clone());
            // A DM carries the sender's profile key (Signal-style); the gateway
            // just stored it. Re-sync the sidebar so list_friends_with_profiles
            // decrypts + persists the peer's real display name + avatar and the
            // row re-renders (fallback "User <id>" → real name, initials →
            // avatar). Refreshes on every DM receive; fine for
            // low-volume friend DMs, revisit if it shows up in profiling.
            // This runs on the runtime, where a Weak never upgrades — the
            // refresh has to be scheduled onto the UI thread.
            if is_server_dm {
                let ui_ctx = ui_ctx.clone();
                crate::run_ui!(window, |w| {
                    crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
                });
            }
        }

        DomainEvent::MessageDeliveryChanged { message_id, status, .. } => {
            messaging::handle_persisted_delivery_status(window, ui_ctx, message_id.to_string(), status);
        }

        // Receipt events: update checkmarks (✓✓) in the Slint message model.
        DomainEvent::MessagesDelivered { message_timestamps, .. } => {
            if matches!(ui_ctx.chat_state().active_conversation(),
                chatter_core::state::ActiveConversation::AnonDm(_) |
                chatter_core::state::ActiveConversation::AnonGroup(_)) {
                messaging::handle_delivery_status_update(window, message_timestamps, "delivered");
            }
        }

        DomainEvent::MessagesRead { message_timestamps, .. } => {
            if matches!(ui_ctx.chat_state().active_conversation(),
                chatter_core::state::ActiveConversation::AnonDm(_) |
                chatter_core::state::ActiveConversation::AnonGroup(_)) {
                messaging::handle_delivery_status_update(window, message_timestamps, "read");
            }
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
        DomainEvent::MessageEdited {
            message_id, new_content, channel_id, author_id, target_sent_timestamp, ..
        } => {
            use chatter_core::domain::message::MessageContent;
            let new_text = match &new_content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
                MessageContent::AI(ai) => ai.text.clone(),
                _ => String::new(),
            };
            messaging::handle_message_edit(
                window, ui_ctx, channel_id, &message_id.to_string(), author_id,
                target_sent_timestamp, &new_text,
            );
        }

        // Delete for everyone: remove the (author, timestamp) bubble from the
        // open view. A DM delete may name no channel; the DM is the author's.
        DomainEvent::MessageDeleted { author_id, channel_id, target_sent_timestamp, .. } => {
            messaging::handle_message_delete(
                window,
                ui_ctx,
                channel_id.unwrap_or_else(|| author_id.into()),
                messaging::DeleteTarget::AuthorTimestamp { author: author_id, sent_at: target_sent_timestamp },
            );
        }

        // A delete-for-me made on a sibling device. Core's state store drops
        // it from storage; the open chat has to drop the bubble too.
        DomainEvent::DeleteForMeSync { conversation_id, target_message_id, .. } => {
            messaging::handle_message_delete(
                window,
                ui_ctx,
                conversation_id,
                messaging::DeleteTarget::Id(target_message_id),
            );
        }

        // Read here: everything in these conversations is read. Also the
        // foreground catch-up (`set_app_visible`), which has no UI caller to
        // clear the badge, so without this the row kept a stale count until
        // the next rebuild.
        DomainEvent::ConversationsRead { conversation_ids, from_other_device: false } => {
            let ui_ctx = ui_ctx.clone();
            crate::run_ui!(window, |w| {
                for id in &conversation_ids {
                    helpers::update_unread_count(&w, &ui_ctx, &id.to_string(), 0);
                }
                crate::bin_ui::features::sidebar::refresh_section_badges(&w, &ui_ctx);
            });
        }
        // Read on another of our devices: core marked the messages read;
        // rows and badges are rebuilt from storage (a sync can read only some).
        DomainEvent::ConversationsRead { from_other_device: true, .. } => {
            crate::bin_ui::features::sidebar::rebuild_chat_list(window, ui_ctx);
            let ui_ctx = ui_ctx.clone();
            crate::run_ui!(window, |w| {
                crate::bin_ui::features::sidebar::refresh_section_badges(&w, &ui_ctx);
                helpers::recompute_total_unread(&w, &ui_ctx);
            });
        }

        // No Slint model update needed
        DomainEvent::ViewedReceipt { .. } => {}

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
                let ui_ctx_dl = ui_ctx.clone();
                let weak = window.clone();
                let conv_id = channel_id.to_string();
                tokio::spawn(async move {
                    super::attachment_download::download_attachments(
                        &downloadable,
                        ui_ctx_dl,
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
            anon::handle_group_member_joined(window, ui_ctx, group_id, user_id);
        }

        DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
            anon::handle_group_member_left(window, ui_ctx, group_id, user_id);
        }

        DomainEvent::AnonGroupMemberKicked { group_id, user_id, kicked_by } => {
            anon::handle_group_member_kicked(window, ui_ctx, group_id, user_id, kicked_by);
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
            // Accepted → anon peer becomes a Friends-tab contact.
            crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                window.clone(),
                ui_ctx.clone(),
            );
        }

        DomainEvent::AnonDmInitReceived { .. } => {
            anon::handle_dm_init_received(window, ui_ctx);
        }

        // =========================================================================
        // Anonymous Call Events
        // =========================================================================
        #[cfg(feature = "call-ui")]
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
        #[cfg(not(feature = "call-ui"))]
        DomainEvent::AnonCallOfferReceived { .. } => {
            tracing::debug!("Ignoring AnonCallOfferReceived (desktop-media disabled)");
        }

        DomainEvent::AnonCallAnswerReceived { .. } => {
            // Handled entirely by the call orchestrator bridge — it calls
            // CallSignalingService::handle_anon_call_answer and emits
            // AnonCallConnected / ActiveEnded orchestrator events.
        }

        DomainEvent::AnonCallHangupReceived { .. }
        | DomainEvent::AnonGroupCallHangupReceived { .. } => {
            // Handled by the call orchestrator bridge: it withdraws the ring
            // or ends the call (1:1 only), and the UI follows its
            // `IncomingWithdrawn` / `ActiveEnded` for that call.
        }

        // =========================================================================
        // Anonymous Group Call Events
        // =========================================================================
        #[cfg(feature = "call-ui")]
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
        #[cfg(not(feature = "call-ui"))]
        DomainEvent::AnonGroupCallOfferReceived { .. } => {
            tracing::debug!("Ignoring AnonGroupCallOfferReceived (desktop-media disabled)");
        }

        DomainEvent::AnonGroupCallAnswerReceived { .. } => {
            // Handled entirely by the call orchestrator bridge (same path as
            // AnonCallAnswerReceived).
        }

        // =========================================================================
        // Message Send Events
        // =========================================================================
        DomainEvent::MessageSendFailed { channel_id, error, message_timestamps } => {
            messaging::handle_message_send_failed(window, channel_id, error, message_timestamps);
        }

        // =========================================================================
        // Channel Member Events
        // =========================================================================
        DomainEvent::ChannelMemberJoined { channel_id, user_id: _, display_name: _, message } => {
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, channel_id);
        }

        DomainEvent::ChannelMemberLeft { channel_id, user_id, display_name: _, message } => {
            // We left, or were kicked or banned: core dropped our membership,
            // so the composer gives way to the notice and the sidebar row is
            // rebuilt from core (its preview, and no "Leave channel" in its menu).
            let we_left = ui_ctx.session_current_user_id() == Some(user_id);
            messaging::handle_message_received(window, ui_ctx, message);
            messaging::update_active_channel_member_count(window, ui_ctx, channel_id);
            if we_left {
                crate::bin_ui::features::chat::refresh_send_notice(window, ui_ctx, channel_id);
                crate::bin_ui::features::sidebar::rebuild_chat_list(window, ui_ctx);
                crate::bin_ui::features::sidebar::sections::refresh_sections_ui(window, ui_ctx);
            }
        }

        // Channel metadata (name/icon) was changed by an admin. Core already
        // persisted the new values to the channel repo; live-refresh the header
        // title if this is the active conversation.
        DomainEvent::ChannelUpdated { channel_id, name, icon: _ } => {
            let is_active = ui_ctx.chat_state().get().active_channel_id
                .map(|id| id == channel_id)
                .unwrap_or(false);
            if is_active {
                let weak = window.clone();
                let new_name = name.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_name(new_name.into());
                    }
                });
            }
        }

        // A member's role or moderator mute changed (server `MemberUpdated`).
        // Core persisted it to the membership repo; refresh the member view and,
        // in case it was our own mute, the composer notice.
        DomainEvent::ChannelMemberUpdated { channel_id, user_id: _, role: _ } => {
            messaging::update_active_channel_member_count(window, ui_ctx, channel_id);
            crate::bin_ui::features::chat::show_send_notice(window, ui_ctx, channel_id);
        }

        // =========================================================================
        // User Presence Updates
        // =========================================================================
        DomainEvent::UserPresenceChanged {
            user_id,
            presence,
            timestamp,
        } => {
            social::handle_presence_changed(window, ui_ctx, user_id, presence, timestamp);
        }

        // =========================================================================
        // Anonymous DM Profile Updates
        // =========================================================================
        DomainEvent::AnonDmProfileUpdated {
            conversation_id,
            user_id,
            display_name,
            avatar,
            avatar_url,
            bio,
            timestamp,
        } => {
            social::handle_anon_dm_profile_updated(
                window, ui_ctx, conversation_id, user_id, display_name, avatar, avatar_url, bio, timestamp,
            );
            // Name/avatar changed — refresh Friends-tab row too.
            crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                window.clone(),
                ui_ctx.clone(),
            );
        }

        // A note-to-self sent from a sibling device. Core stored it; show it
        // like any received message (inserted only if Saved Messages is open).
        DomainEvent::SavedMessageSynced { message } => {
            messaging::handle_message_received(window, ui_ctx, message);
        }

        // Jarvis AI events — UI can subscribe directly for display
        DomainEvent::JarvisResponse { .. }
        | DomainEvent::JarvisStats { .. }
        | DomainEvent::JarvisCleared { .. } => {}

        // The catch-up sync stores channel messages without a MessageReceived
        // each (a backfill must not fire a toast per message), so a message
        // missed while the stream was down — app backgrounded — reached the
        // conversation but never its row. Rebuild the rows from storage, the
        // way every DM receive already does (`refresh_chat_list`).
        DomainEvent::SyncComplete { messages_synced, blocks_changed, .. } => {
            // Friends were painted from storage at startup. This cycle is
            // the reconcile: membership and live presence, or the same
            // stored rows if the server is still down. A block change also
            // rebuilds the Blocked tab (that list is already local).
            crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                window.clone(), ui_ctx.clone(),
            );
            if blocks_changed {
                crate::bin_ui::features::modals::friends::refresh_blocked_users(window.clone(), ui_ctx);
            }
            if messages_synced > 0 || blocks_changed {
                crate::bin_ui::features::sidebar::rebuild_chat_list(window, ui_ctx);
                let ui_ctx = ui_ctx.clone();
                crate::run_ui!(window, |w| {
                    crate::bin_ui::features::sidebar::refresh_section_badges(&w, &ui_ctx);
                    helpers::recompute_total_unread(&w, &ui_ctx);
                });
            }
        }

        // Multi-device lifecycle: a sibling device just joined this account.
        // Log it (OS toast already fired from core via NotificationService)
        // and refresh the sessions/devices modal if it's currently open so
        // the new entry appears without manual re-fetch.
        DomainEvent::DeviceLinked { device_name, approved_by_self, .. } => {
            sessions::handle_device_linked(window, ui_ctx, device_name, approved_by_self);
        }
        // Events with no UI render — core handles the work, UI doesn't
        // surface anything to the user.
        DomainEvent::PushWakeup { .. } => {}
        // A call was missed mid-session: repaint the Calls tab badge now, not
        // at the next restart or Calls open.
        DomainEvent::MissedCallsChanged => {
            let missed = ui_ctx.missed_call_count().await as i32;
            crate::run_ui!(window, |w| w.set_missed_call_count(missed));
        }
        DomainEvent::DmSessionHealed { .. } => {}
        DomainEvent::PeerDevicesChanged { .. } => {}
        // A peer's identity key changed while we were running. If their
        // conversation is the one on screen, raise the prompt now rather than
        // at the next open — the user is likely mid-typing into a DM that has
        // just stopped being able to send.
        DomainEvent::PeerIdentityChanged { peer_user_id, .. } => {
            let ui_ctx = ui_ctx.clone();
            let _ = window.upgrade_in_event_loop(move |w| {
                if ui_ctx.chat_state().active_conversation()
                    == (chatter_core::state::ActiveConversation::ServerDm { peer_id: peer_user_id })
                {
                    crate::bin_ui::features::chat::identity_change::refresh(
                        &w, &ui_ctx, &peer_user_id,
                    );
                }
            });
        }
        // Server-managed profile updated. Core already upserted user_repo.
        // Refresh display_name + status across sidebar / header / profile
        // modal in real time, then refresh friends-list (Friends tab).
        DomainEvent::UserProfileUpdated {
            user_id,
            display_name,
            status_message,
            avatar_bytes,
            timestamp,
        } => {
            social::handle_user_profile_updated(
                window, ui_ctx, user_id, display_name, status_message, avatar_bytes, timestamp,
            );
            crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                window.clone(),
                ui_ctx.clone(),
            );
            // Rebuild the DM sidebar from core so the friend's decoded avatar
            // (not just display_name/initials, which handle_user_profile_updated
            // already patches in-place) renders too — covers the profile-key
            // handshake path where this is the very first event either side
            // sees about the other's real profile. On the UI thread: a Weak
            // never upgrades on the runtime this handler runs on.
            let ui_ctx = ui_ctx.clone();
            crate::run_ui!(window, |w| {
                crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            });
        }
        // =========================================================================
        // Channel Lifecycle (INFRA-29)
        // =========================================================================
        // Core has already written the authoritative local state for each of
        // these; the UI's job is to stop showing what is no longer true.

        DomainEvent::ChannelCreated { .. } | DomainEvent::ChannelArchivedChanged { .. } => {
            crate::bin_ui::features::auth::load_initial_data(window.clone(), ui_ctx).await;
        }

        DomainEvent::ChannelModerationAction { channel_id, action, reason, is_self, .. } => {
            if is_self {
                // An open chat stays open: it is read-only now, its composer
                // replaced by the notice (ChannelMemberLeft refreshes both).
                let removed = matches!(action.as_str(), "KICK" | "BAN");
                let text = match (action.as_str(), reason.as_deref()) {
                    ("BAN", Some(r)) => format!("You were banned from this channel: {r}"),
                    ("BAN", None) => "You were banned from this channel.".to_string(),
                    ("KICK", Some(r)) => format!("You were removed from this channel: {r}"),
                    ("KICK", None) => "You were removed from this channel.".to_string(),
                    ("MUTE", _) => "You were muted in this channel.".to_string(),
                    ("UNMUTE", _) => "You were unmuted in this channel.".to_string(),
                    ("UNBAN", _) => "Your ban was lifted.".to_string(),
                    _ => "A moderation action was applied to your account.".to_string(),
                };
                let weak = window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_toast_message(text.into());
                        w.set_toast_variant("error".into());
                        w.set_toast_visible(true);
                    }
                });
                if removed {
                    crate::bin_ui::features::auth::load_initial_data(window.clone(), ui_ctx).await;
                }
            } else {
                // Someone else was moderated: the roster changed.
                messaging::update_active_channel_member_count(window, ui_ctx, channel_id);
            }
        }

        // A received invite only changes the invites list — on reconnect core
        // re-announces every pending one, and a full reload per invite was N
        // reloads. A response (a join) changes the channel list too.
        DomainEvent::ChannelInviteReceived { .. } => {
            crate::bin_ui::features::modals::load_channel_invites(window.clone(), ui_ctx);
        }
        DomainEvent::ChannelInviteResponded { .. } => {
            crate::bin_ui::features::modals::load_channel_invites(window.clone(), ui_ctx);
            crate::bin_ui::features::auth::load_initial_data(window.clone(), ui_ctx).await;
        }

        // Core decided what the notice says (`MembershipNotice::user_text`).
        DomainEvent::ChannelMembershipNotice { channel_id, notice } => {
            // The banner (and the send block it explains) may have changed.
            crate::bin_ui::features::chat::show_send_notice(window, ui_ctx, channel_id);
            let text = notice.user_text();
            let variant = if notice.is_problem() { "error" } else { "success" };
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_toast_message(text.into());
                    w.set_toast_variant(variant.into());
                    w.set_toast_visible(true);
                }
            });
        }

        // The saga said no. Core already re-read the channel from the server,
        // so a refresh drops whatever optimistic row this client was showing;
        // the toast is what stops the user waiting for an operation that is
        // never going to land.
        DomainEvent::ChannelOperationFailed { code, message, .. } => {
            let text = if message.is_empty() {
                format!("Channel operation failed ({code}).")
            } else {
                format!("Channel operation failed: {message}")
            };
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_toast_message(text.into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                }
            });
            crate::bin_ui::features::auth::load_initial_data(window.clone(), ui_ctx).await;
        }
    }
}
