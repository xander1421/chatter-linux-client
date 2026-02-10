//! Chat callbacks.
//!
//! Handles sending messages to channels and DMs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use prost::Message as ProstMessage;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::crypto::encrypt_dm_bytes;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::UserId;
use chatter_linux::mailbox::dm::DmConversationManager;
use chatter_linux::network::grpc::auth_client::AuthClient;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::message_client::MessageClient;
use chatter_linux::proto::content_v1::Content;
use chatter_linux::proto::mailbox_v1::EnvelopeType;
use chatter_linux::services::{
    build_text_content, build_typing_content, build_typing_content_with_channel,
    build_reaction_content, build_reaction_content_with_channel,
    build_read_receipt_content, build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
    SignalService, MailboxManager, MessageService,
};
use chatter_linux::storage::Database;
use chatter_linux::network::grpc::GrpcClientPool;

use crate::bin_ui::converters::message_to_display_data;
use crate::{MainWindow, PendingAttachmentData};

// ============================================================================
// DM Content Sender Helper
// ============================================================================

/// Send any Content proto to a DM peer via sealed sender + mailbox.
async fn send_dm_content(
    content: Content,
    peer_id: &UserId,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    signal_service: Arc<SignalService>,
    mailbox_manager: Arc<MailboxManager>,
) -> Result<(), String> {
    let conversation_manager = DmConversationManager::new(database);

    // Load conversation
    let conversation = conversation_manager
        .load_conversation(peer_id)
        .map_err(|e| format!("Failed to load conversation: {}", e))?
        .ok_or_else(|| format!("No conversation with {}", peer_id))?;

    if !conversation.is_ready_to_send() {
        return Err("Conversation not ready".into());
    }

    let peer_mailbox = conversation.peer_mailbox
        .as_ref()
        .ok_or("No peer mailbox")?;

    let shared_secret = conversation.conversation_key
        .as_ref()
        .and_then(|k| k.get_shared_secret())
        .ok_or("No shared secret")?;

    // Serialize Content proto
    let plaintext = content.encode_to_vec();

    // Layer 1: Signal Protocol encryption
    let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
    let peer_device_id = 1u32;
    let peer_id_str = peer_id.to_string();

    let signal_ciphertext = signal_service
        .encrypt_message(&mut auth_client, &peer_id_str, peer_device_id, &plaintext)
        .await
        .map_err(|e| format!("Signal encrypt failed: {}", e))?;

    // Layer 2: Mailbox encryption
    let mailbox_encrypted = encrypt_dm_bytes(&signal_ciphertext, shared_secret)
        .map_err(|e| format!("Mailbox encrypt failed: {}", e))?;

    // Send to mailbox
    mailbox_manager
        .send_to_mailbox(&peer_mailbox.public_key, mailbox_encrypted, EnvelopeType::SealedSender)
        .await
        .map_err(|e| format!("Send to mailbox failed: {}", e))?;

    Ok(())
}

/// Send read receipts for messages in a DM conversation.
///
/// Call this when a DM conversation is opened to mark messages as read.
pub fn send_dm_read_receipts(
    peer_id: UserId,
    message_timestamps: Vec<u64>,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    signal_service: Arc<SignalService>,
    mailbox_manager: Arc<MailboxManager>,
) {
    if message_timestamps.is_empty() {
        return;
    }

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime for read receipt");

        rt.block_on(async {
            let content = build_read_receipt_content(message_timestamps.clone());

            if let Err(e) = send_dm_content(
                content,
                &peer_id,
                database,
                grpc_pool,
                signal_service,
                mailbox_manager,
            )
            .await
            {
                tracing::debug!("Failed to send read receipts: {}", e);
            } else {
                tracing::debug!(
                    "Sent read receipts for {} messages to peer {}",
                    message_timestamps.len(),
                    peer_id
                );
            }
        });
    });
}

/// Send encrypted Content proto to a channel via sender key.
///
/// Used for control messages (typing, reactions, edits, etc.) in encrypted channels.
async fn send_channel_content(
    content: Content,
    channel_id: &ChannelId,
    message_service: Arc<MessageService>,
    grpc_pool: Arc<GrpcClientPool>,
) -> Result<(), String> {
    let mut message_client = MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
    let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    message_service
        .send_control_content(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            content,
        )
        .await
        .map_err(|e| format!("Failed to send channel content: {}", e))
}

/// Wire chat callbacks (send message).
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    let message_service = ctx.message_service();
    let chat_state = ctx.chat_state.clone();
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let channel_repo = ctx.channel_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let signal_service = ctx.signal_service.clone();

    let weak_window = window.as_weak();

    window.on_send(move |text: SharedString| {
        let text = text.to_string();
        let chat_state_check = chat_state.clone();
        let has_attachments = !chat_state_check.get_pending_attachments().is_empty();

        // Must have text OR attachments to send
        if text.trim().is_empty() && !has_attachments {
            return;
        }

        let message_service = message_service.clone();
        let chat_state = chat_state.clone();
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let mailbox_manager = mailbox_manager.clone();
        let database = database.clone();
        let attachment_repo = attachment_repo.clone();
        let weak = weak_window.clone();

        // Get the selected DM ID from UI (if any)
        let selected_dm_id = weak
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        // Check if this is a DM or channel message
        let is_dm = !selected_dm_id.is_empty();

        if is_dm {
            // Send via mailbox for DM privacy with Signal Protocol forward secrecy
            // Use spawn_blocking because libsignal stores aren't Send
            let signal_service = signal_service.clone();
            let grpc_pool = grpc_pool.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for DM send");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        tracing::warn!("Cannot send DM: not logged in");
                        return;
                    }

                    // Get the conversation manager to find peer's mailbox
                    let conversation_manager =
                        chatter_linux::mailbox::dm::DmConversationManager::new(database);

                    let peer_id: chatter_linux::domain::UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid DM peer ID: {}", e);
                            return;
                        }
                    };

                    // Load conversation to get peer's mailbox
                    let conversation = match conversation_manager.load_conversation(&peer_id) {
                        Ok(Some(conv)) => conv,
                        Ok(None) => {
                            tracing::error!(
                                "No DM conversation found for peer {}. Start a DM first.",
                                peer_id
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to load DM conversation: {}", e);
                            return;
                        }
                    };

                    // Check if conversation is ready to send
                    if !conversation.is_ready_to_send() {
                        tracing::warn!(
                            "DM conversation with {} is not ready to send. \
                             Waiting for key exchange to complete.",
                            peer_id
                        );
                        return;
                    }

                    // Get peer's mailbox public key
                    let peer_mailbox = match &conversation.peer_mailbox {
                        Some(m) => m,
                        None => {
                            tracing::error!("No peer mailbox address for DM with {}", peer_id);
                            return;
                        }
                    };

                    // Get shared secret for mailbox layer encryption
                    let shared_secret = match conversation
                        .conversation_key
                        .as_ref()
                        .and_then(|k| k.get_shared_secret())
                    {
                        Some(secret) => *secret,
                        None => {
                            tracing::error!("No shared secret for DM with {}", peer_id);
                            return;
                        }
                    };

                    // Wrap text in Content proto (uses build_text_content helper)
                    let content = build_text_content(&text);
                    let plaintext = content.encode_to_vec();

                    // Layer 1: Signal Protocol encryption (forward secrecy via Double Ratchet)
                    let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                        grpc_pool.channel(),
                        grpc_pool.auth_clone(),
                    );

                    let peer_device_id = 1u32; // Primary device
                    let peer_id_str = peer_id.to_string();

                    let signal_ciphertext = match signal_service
                        .encrypt_message(&mut auth_client, &peer_id_str, peer_device_id, &plaintext)
                        .await
                    {
                        Ok(ct) => {
                            tracing::debug!(
                                "Signal encrypted DM for peer {} ({} bytes)",
                                peer_id,
                                ct.len()
                            );
                            ct
                        }
                        Err(e) => {
                            tracing::error!("Failed to Signal encrypt DM message: {}", e);
                            return;
                        }
                    };

                    // Layer 2: Mailbox encryption (sender anonymity)
                    use chatter_linux::crypto::encrypt_dm_bytes;
                    let mailbox_encrypted = match encrypt_dm_bytes(&signal_ciphertext, &shared_secret) {
                        Ok(enc) => enc,
                        Err(e) => {
                            tracing::error!("Failed to mailbox encrypt DM: {}", e);
                            return;
                        }
                    };

                    // Send to peer's mailbox
                    match mailbox_manager
                        .send_to_mailbox(
                            &peer_mailbox.public_key,
                            mailbox_encrypted,
                            EnvelopeType::SealedSender,
                        )
                        .await
                    {
                        Ok(()) => {
                            tracing::debug!("DM sent to peer {} via mailbox with Signal encryption", peer_id);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_composer_text("".into());
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to send DM via mailbox: {}", e);
                        }
                    }
                });
            });
        } else {
            // Send via regular channel message flow
            let channel_repo = channel_repo.clone();
            let user_repo = user_repo.clone();
            let attachment_repo = attachment_repo.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for message send");

                rt.block_on(async {
                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Cannot send message: no active channel");
                            return;
                        }
                    };

                    if !auth_state.is_logged_in() {
                        tracing::warn!("Cannot send message: not logged in");
                        return;
                    }

                    // Check if this is a SavedMessages channel
                    let is_saved_messages = match channel_repo.get(&channel_id) {
                        Ok(Some(channel)) => channel.channel_type == ChannelType::SavedMessages,
                        _ => false,
                    };

                    let mut message_client =
                        MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                    let mut auth_client =
                        chatter_linux::network::grpc::auth_client::AuthClient::new(
                            grpc_pool.channel(),
                            grpc_pool.auth_clone(),
                        );

                    let mut channel_client =
                        ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                    // Get pending attachments from chat state
                    let pending_attachment_ids = chat_state.get_pending_attachment_ids();
                    tracing::debug!(
                        "Sending message with {} attachments",
                        pending_attachment_ids.len()
                    );

                    if is_saved_messages {
                        // Use Signal-style Note to Self handling
                        match message_service
                            .send_saved_message(
                                &mut auth_client,
                                &mut message_client,
                                &text,
                                pending_attachment_ids.clone(),
                            )
                            .await
                        {
                            Ok(message) => {
                                // Clear pending attachments after successful send
                                chat_state.clear_pending_attachments();

                                // Get sender name for display
                                let sender_name = if let Some(sender_id) = &message.sender_id {
                                    user_repo
                                        .get_profile(sender_id)
                                        .ok()
                                        .flatten()
                                        .map(|p| p.display_name)
                                        .or_else(|| {
                                            user_repo.get(sender_id).ok().flatten().map(|u| u.username)
                                        })
                                        .unwrap_or_else(|| "Me".to_string())
                                } else {
                                    "Me".to_string()
                                };

                                // Resolve attachments for display
                                let resolved_attachments: Vec<_> = message
                                    .attachments
                                    .iter()
                                    .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                                    .collect();

                                // Convert to display data (thread-safe)
                                let display_msg = message_to_display_data(&message, &sender_name, &resolved_attachments);

                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        // Convert to Slint type on UI thread
                                        let slint_msg = display_msg.to_slint();

                                        // Add message to the end of the list
                                        let current_model = w.get_messages();
                                        let mut messages: Vec<_> = (0..current_model.row_count())
                                            .filter_map(|i| current_model.row_data(i))
                                            .collect();
                                        messages.push(slint_msg);
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        // Clear attachment tray UI
                                        w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                                        w.set_composer_placeholder("Type a message...".into());
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Failed to send saved message: {}", e);
                            }
                        }
                    } else {
                        // Regular channel message
                        match message_service
                            .send_message(
                                &mut message_client,
                                &mut auth_client,
                                &mut channel_client,
                                &channel_id,
                                &text,
                                pending_attachment_ids.clone(),
                                None,
                                vec![],
                                false,
                            )
                            .await
                        {
                            Ok(message) => {
                                // Clear pending attachments after successful send
                                chat_state.clear_pending_attachments();

                                // Get sender name for display
                                let sender_name = if let Some(sender_id) = &message.sender_id {
                                    user_repo
                                        .get_profile(sender_id)
                                        .ok()
                                        .flatten()
                                        .map(|p| p.display_name)
                                        .or_else(|| {
                                            user_repo.get(sender_id).ok().flatten().map(|u| u.username)
                                        })
                                        .unwrap_or_else(|| "Me".to_string())
                                } else {
                                    "Me".to_string()
                                };

                                // Resolve attachments for display
                                let resolved_attachments: Vec<_> = message
                                    .attachments
                                    .iter()
                                    .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                                    .collect();

                                // Convert to display data (thread-safe)
                                let display_msg = message_to_display_data(&message, &sender_name, &resolved_attachments);

                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        // Convert to Slint type on UI thread
                                        let slint_msg = display_msg.to_slint();

                                        // Add message to the end of the list
                                        let current_model = w.get_messages();
                                        let mut messages: Vec<_> = (0..current_model.row_count())
                                            .filter_map(|i| current_model.row_data(i))
                                            .collect();
                                        messages.push(slint_msg);
                                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                                        w.set_composer_text("".into());
                                        // Clear attachment tray UI
                                        w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                                        w.set_composer_placeholder("Type a message...".into());
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Failed to send message: {}", e);
                            }
                        }
                    }
                });
            });
        }
    });

    // Wire typing indicator callback
    wire_typing_changed(window, ctx);

    // Wire reaction selected callback
    wire_reaction_selected(window, ctx);

    // Wire context menu callbacks
    wire_more_clicked(window, ctx);
    wire_reply_message(window, ctx);
    wire_edit_message(window, ctx);
    wire_send_edit(window, ctx);
    wire_cancel_edit(window);
    wire_delete_message(window, ctx);
    wire_delete_everyone_message(window, ctx);
    wire_pin_message(window, ctx);
    wire_forward_message_action(window, ctx);
    wire_copy_message(window, ctx);
}

/// Wire typing indicator callback for DM and channel conversations.
fn wire_typing_changed(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let signal_service = ctx.signal_service.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    // Track typing state to avoid sending duplicate indicators
    let was_typing = Arc::new(AtomicBool::new(false));

    let weak_window = window.as_weak();

    window.on_typing_changed(move |is_typing: bool| {
        // Skip if state hasn't changed
        let prev_typing = was_typing.swap(is_typing, Ordering::SeqCst);
        if prev_typing == is_typing {
            return;
        }

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();

        // DM typing indicator via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let signal_service = signal_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for typing indicator");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Build typing indicator content
                    let content = build_typing_content(is_typing);

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database,
                        grpc_pool,
                        signal_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::debug!("Failed to send DM typing indicator: {}", e);
                    }
                });
            });
        } else {
            // Channel typing indicator via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel typing indicator");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Build typing indicator content with channel context
                    let content = build_typing_content_with_channel(is_typing, Some(&channel_id.to_string()));

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::debug!("Failed to send channel typing indicator: {}", e);
                    }
                });
            });
        }
    });
}

/// Wire reaction selected callback for DM and channel conversations.
fn wire_reaction_selected(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let signal_service = ctx.signal_service.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_reaction_selected(move |message_id: SharedString, emoji: SharedString| {
        let message_id_str = message_id.to_string();
        let emoji_str = emoji.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();

        // DM reaction via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let signal_service = signal_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for reaction send");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for reaction: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get sender and timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for reaction: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for reaction: {}", e);
                            return;
                        }
                    };

                    // Get the message author (target_author_aci)
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();

                    // Get the message timestamp (milliseconds since epoch)
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build reaction content
                    // remove=false means adding a reaction
                    let content = build_reaction_content(
                        &emoji_str,
                        false,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database,
                        grpc_pool,
                        signal_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM reaction: {}", e);
                    } else {
                        tracing::debug!(
                            "Sent DM reaction {} to message {} (author: {}, ts: {}) for peer {}",
                            emoji_str, msg_id, target_author_aci, target_sent_timestamp, peer_id
                        );
                    }
                });
            });
        } else {
            // Channel reaction via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel reaction send");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for channel reaction: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get sender and timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel reaction: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel reaction: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build reaction content with channel context
                    let content = build_reaction_content_with_channel(
                        &emoji_str,
                        false,
                        &target_author_aci,
                        target_sent_timestamp,
                        Some(&channel_id.to_string()),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel reaction: {}", e);
                    } else {
                        tracing::debug!(
                            "Sent channel reaction {} to message {} in channel {}",
                            emoji_str, msg_id, channel_id
                        );
                    }
                });
            });
        }
    });
}

/// Wire more-clicked callback to show context menu.
fn wire_more_clicked(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let auth_state = ctx.auth_state.clone();

    let weak_window = window.as_weak();

    window.on_more_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();
        let auth_state = auth_state.clone();
        let weak = weak_window.clone();

        // Parse message ID and look up message info
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for context menu: {}", e);
                return;
            }
        };

        // Look up the message to determine ownership and pin status
        let message = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::error!("Message not found for context menu: {}", msg_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to look up message for context menu: {}", e);
                return;
            }
        };

        // Check if this is the current user's own message
        let current_user_id = auth_state.get().current_user_id.clone();
        let is_own_message = message.sender_id == current_user_id;

        // Check if message is pinned
        let is_pinned = message.is_pinned;

        // Show context menu
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_context_menu_message_id(message_id_str.into());
                w.set_context_menu_is_own_message(is_own_message);
                w.set_context_menu_is_pinned(is_pinned);
                // Position in center (fixed values work since menu uses full screen backdrop)
                w.set_context_menu_x(400.0);
                w.set_context_menu_y(300.0);
                w.set_context_menu_visible(true);
            }
        });
    });
}

/// Wire edit-message callback - enters edit mode with original message text.
fn wire_edit_message(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let weak_window = window.as_weak();

    window.on_edit_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // Parse message ID and look up message
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for edit: {}", e);
                return;
            }
        };

        // Look up the message to get original text
        let message = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::error!("Message not found for edit: {}", msg_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to look up message for edit: {}", e);
                return;
            }
        };

        // Extract original text
        use chatter_linux::domain::message::MessageContent;
        let original_text = match &message.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
            _ => {
                tracing::debug!("Cannot edit non-text message content");
                return;
            }
        };

        // Enter edit mode
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_composer_text(original_text.into());
                w.set_composer_edit_message_id(message_id_str.into());
                w.set_composer_is_editing(true);
                tracing::debug!("Entered edit mode for message: {}", msg_id);
            }
        });
    });
}

/// Wire send-edit callback - sends EditMessage via sealed sender (DM) or sender key (channel).
fn wire_send_edit(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let signal_service = ctx.signal_service.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_send_edit(move |message_id: SharedString, new_text: SharedString| {
        let message_id_str = message_id.to_string();
        let new_text_str = new_text.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // DM edit via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let signal_service = signal_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for edit message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for edit: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get original timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for edit: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for edit: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build edit content
                    let content = build_edit_content(target_sent_timestamp, &new_text_str);

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        grpc_pool,
                        signal_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM edit: {}", e);
                        return;
                    }

                    // Update locally
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated_message = message.clone();
                    updated_message.content = MessageContent::Text(new_text_str.clone());
                    updated_message.is_edited = true;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update message locally: {}", e);
                    } else {
                        tracing::debug!("Edited DM message {} for peer {}", msg_id, peer_id);
                    }

                    // Exit edit mode and clear composer
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_composer_is_editing(false);
                            w.set_composer_edit_message_id("".into());
                            w.set_composer_text("".into());
                        }
                    });
                });
            });
        } else {
            // Channel edit via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel edit message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for channel edit: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get original timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel edit: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel edit: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build edit content with channel context
                    let content = build_edit_content_with_channel(
                        target_sent_timestamp,
                        &new_text_str,
                        Some(&channel_id.to_string()),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel edit: {}", e);
                        return;
                    }

                    // Update locally
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated_message = message.clone();
                    updated_message.content = MessageContent::Text(new_text_str.clone());
                    updated_message.is_edited = true;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update message locally: {}", e);
                    } else {
                        tracing::debug!("Edited channel message {} in channel {}", msg_id, channel_id);
                    }

                    // Exit edit mode and clear composer
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_composer_is_editing(false);
                            w.set_composer_edit_message_id("".into());
                            w.set_composer_text("".into());
                        }
                    });
                });
            });
        }
    });
}

/// Wire cancel-edit callback - exits edit mode without sending.
fn wire_cancel_edit(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_edit(move || {
        let _ = slint::invoke_from_event_loop({
            let weak = weak_window.clone();
            move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                    w.set_composer_text("".into());
                    tracing::debug!("Cancelled edit mode");
                }
            }
        });
    });
}

/// Wire delete-message callback for DM and channel conversations.
/// Delete-for-me: deletes locally and syncs to own devices, does not notify peer/channel.
fn wire_delete_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let signal_service = ctx.signal_service.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_delete_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // DM delete-for-me via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let signal_service = signal_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for delete message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for delete: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for delete: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for delete: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;
                    let conversation_id = peer_id.to_string();

                    // Build delete-for-me content (syncs to own devices)
                    let content = build_delete_for_me_content(
                        &conversation_id,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        grpc_pool,
                        signal_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM delete-for-me: {}", e);
                        return;
                    }

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!("Failed to delete message locally: {}", e);
                    } else {
                        tracing::debug!("Deleted DM message {} for peer {}", msg_id, peer_id);

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        } else {
            // Channel delete-for-me via sender key (syncs to own devices)
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel delete message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for channel delete: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel delete: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel delete: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;
                    let conversation_id = channel_id.to_string();

                    // Build delete-for-me content (syncs to own devices)
                    let content = build_delete_for_me_content(
                        &conversation_id,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel delete-for-me: {}", e);
                        return;
                    }

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!("Failed to delete message locally: {}", e);
                    } else {
                        tracing::debug!("Deleted channel message {} in channel {}", msg_id, channel_id);

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        }
    });
}

/// Wire delete-everyone-message callback for DM and channel conversations.
/// Sends delete notification to peer/channel so they remove the message too.
fn wire_delete_everyone_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let signal_service = ctx.signal_service.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_delete_everyone_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // DM delete-for-everyone via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let signal_service = signal_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for delete-everyone message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for delete-everyone: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for delete-everyone: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for delete-everyone: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build delete-for-everyone content
                    let content = build_delete_for_everyone_content(target_sent_timestamp);

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        grpc_pool,
                        signal_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM delete-everyone: {}", e);
                        return;
                    }

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!("Failed to delete message locally: {}", e);
                    } else {
                        tracing::debug!("Deleted DM message {} for everyone (notified peer {})", msg_id, peer_id);

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        } else {
            // Channel delete-for-everyone via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel delete-everyone message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for channel delete-everyone: {}", e);
                            return;
                        }
                    };

                    // Look up the message to get timestamp
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel delete-everyone: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel delete-everyone: {}", e);
                            return;
                        }
                    };

                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Build delete-for-everyone content with channel context
                    let channel_id_str = channel_id.to_string();
                    let content = build_delete_for_everyone_content_with_channel(
                        target_sent_timestamp,
                        Some(&channel_id_str),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel delete-everyone: {}", e);
                        return;
                    }

                    // Delete locally
                    if let Err(e) = message_repo.delete(&msg_id) {
                        tracing::error!("Failed to delete message locally: {}", e);
                    } else {
                        tracing::debug!("Deleted channel message {} for everyone in channel {}", msg_id, channel_id);

                        // Update UI to remove message
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .filter(|m| m.message_id.as_str() != message_id_str.as_str())
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        }
    });
}

/// Wire pin-message callback for DM and channel conversations.
fn wire_pin_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let signal_service = ctx.signal_service.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();

    let weak_window = window.as_weak();

    window.on_pin_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();

        // Get selected DM ID
        let selected_dm_id = weak_window
            .upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let message_repo = message_repo.clone();

        // DM pin via sealed sender
        if !selected_dm_id.is_empty() {
            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let signal_service = signal_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for pin message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match selected_dm_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for pin: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for pin: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for pin: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Toggle pin state
                    let unpin = message.is_pinned;
                    let action = if unpin { "unpin" } else { "pin" };

                    // Build pin content
                    let content = build_pin_content(
                        &target_author_aci,
                        target_sent_timestamp,
                        unpin,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        grpc_pool,
                        signal_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!("Failed to send DM pin: {}", e);
                        return;
                    }

                    // Update locally
                    let mut updated_message = message.clone();
                    updated_message.is_pinned = !unpin;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update pin status locally: {}", e);
                    } else {
                        tracing::debug!("{} DM message {} for peer {}", action, msg_id, peer_id);
                    }
                });
            });
        } else {
            // Channel pin via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel pin message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse message ID
                    let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid message ID for channel pin: {}", e);
                            return;
                        }
                    };

                    // Look up the message
                    let message = match message_repo.get(&msg_id) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!("Message not found for channel pin: {}", msg_id);
                            return;
                        }
                        Err(e) => {
                            tracing::error!("Failed to look up message for channel pin: {}", e);
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

                    // Toggle pin state
                    let unpin = message.is_pinned;
                    let action = if unpin { "unpin" } else { "pin" };

                    // Build pin content with channel context
                    let channel_id_str = channel_id.to_string();
                    let content = build_pin_content_with_channel(
                        &target_author_aci,
                        target_sent_timestamp,
                        unpin,
                        Some(&channel_id_str),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        grpc_pool,
                    )
                    .await
                    {
                        tracing::error!("Failed to send channel pin: {}", e);
                        return;
                    }

                    // Update locally
                    let mut updated_message = message.clone();
                    updated_message.is_pinned = !unpin;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!("Failed to update pin status locally: {}", e);
                    } else {
                        tracing::debug!("{} channel message {} in channel {}", action, msg_id, channel_id);
                    }
                });
            });
        }
    });
}

/// Wire reply-message callback from context menu.
/// This mirrors the behavior of reply_clicked in chat_actions.rs.
fn wire_reply_message(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();

    window.on_reply_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let message_repo = message_repo.clone();

        tokio::spawn(async move {
            let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid message ID for reply: {}", e);
                    return;
                }
            };

            let reply_preview = match message_repo.get(&msg_id) {
                Ok(Some(msg)) => {
                    let sender = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                    let content = msg.as_text().unwrap_or("").to_string();
                    let preview = if content.len() > 50 {
                        format!("{}...", &content[..47])
                    } else {
                        content
                    };
                    format!("Reply to {}: {}", sender, preview)
                }
                _ => "Reply to message".to_string(),
            };

            let placeholder = reply_preview.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_placeholder(placeholder.into());
                    tracing::info!("Reply mode activated for message: {}", msg_id);
                }
            });
        });
    });
}

/// Wire forward-message-action callback from context menu.
/// Opens the forward modal with the selected message.
fn wire_forward_message_action(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let weak_window = window.as_weak();

    window.on_forward_message_action(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // Look up message to show preview in forward modal
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for forward: {}", e);
                return;
            }
        };

        let preview = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => {
                let content = msg.as_text().unwrap_or("").to_string();
                if content.len() > 100 {
                    format!("{}...", &content[..97])
                } else {
                    content
                }
            }
            _ => "Message".to_string(),
        };

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_forward_message_preview(preview.into());
                w.set_active_modal("forward-message".into());
                tracing::debug!("Forward modal opened for message: {}", message_id_str);
            }
        });
    });
}

/// Wire copy-message callback to copy message text to clipboard.
fn wire_copy_message(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();

    window.on_copy_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();

        // Parse message ID
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for copy: {}", e);
                return;
            }
        };

        // Look up the message
        let message = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::error!("Message not found for copy: {}", msg_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to look up message for copy: {}", e);
                return;
            }
        };

        // Extract text from message content
        use chatter_linux::domain::message::MessageContent;
        let text = match &message.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Mixed(m) => match &m.text {
                Some(t) => t.clone(),
                None => {
                    tracing::debug!("Mixed content has no text");
                    return;
                }
            },
            _ => {
                tracing::debug!("Cannot copy non-text message content");
                return;
            }
        };

        // Copy text to clipboard using arboard
        std::thread::spawn(move || {
            match arboard::Clipboard::new() {
                Ok(mut clipboard) => {
                    if let Err(e) = clipboard.set_text(&text) {
                        tracing::error!("Failed to copy to clipboard: {}", e);
                    } else {
                        tracing::debug!("Copied message text to clipboard");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to access clipboard: {}", e);
                }
            }
        });
    });
}
