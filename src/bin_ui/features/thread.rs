//! Thread panel callbacks.
//!
//! Handles thread panel actions (close, send reply, load more replies).

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::{ChannelId, MessageId};
use chatter_linux::network::grpc::auth_client::AuthClient;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::message_client::MessageClient;
use chatter_linux::state::ChatState;
use chatter_linux::storage::{AttachmentRepository, MessageRepository, UserRepository};

use crate::bin_ui::converters::{message_to_display_data, MessageDisplayData};
use crate::MainWindow;

/// Wire thread panel callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_close_thread(window, ctx);
    wire_send_thread_reply(window, ctx);
    wire_load_more_thread_replies(window, ctx);
}

fn wire_close_thread(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();

    window.on_close_thread_clicked(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();

        // Get active thread and close it
        if let Some(thread) = chat_state.active_thread() {
            chat_state.close_thread(thread.root_message_id);
            tracing::info!("Closed thread: {}", thread.root_message_id);
        }

        // Update UI
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_thread_panel_visible(false);
                w.set_thread_replies(ModelRc::new(VecModel::default()));
            }
        });
    });
}

fn wire_send_thread_reply(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service.clone();
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_send_thread_reply(move |text: SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() {
            return;
        }

        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_service = message_service.clone();
        let grpc_pool = grpc_pool.clone();

        // Use spawn_blocking with nested runtime for non-Send Signal futures
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for thread reply");

            rt.block_on(async {
                // Get active thread info
                let thread = match chat_state.active_thread() {
                    Some(t) => t,
                    None => {
                        tracing::warn!("Cannot send thread reply: no active thread");
                        return;
                    }
                };

                tracing::info!(
                    "Sending thread reply to {} in channel {}",
                    thread.root_message_id,
                    thread.channel_id
                );

                // Create gRPC clients
                let mut message_client =
                    MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
                let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
                let mut channel_client =
                    ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                // Send the message as a reply to the thread root
                match message_service
                    .send_message(
                        &mut message_client,
                        &mut auth_client,
                        &mut channel_client,
                        &thread.channel_id,
                        &text,
                        vec![],                          // no attachments
                        Some(&thread.root_message_id),   // thread_id
                        vec![],                          // no mentioned integrations
                        false,                           // not targeting AI bot
                    )
                    .await
                {
                    Ok(msg) => {
                        tracing::info!("Thread reply sent: {}", msg.id);

                        // Update reply count
                        chat_state.update_thread_reply_count(
                            thread.root_message_id,
                            thread.reply_count + 1,
                        );

                        // Clear composer and refresh
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_thread_composer_text(SharedString::default());
                                // Increment reply count in UI
                                let count = w.get_thread_reply_count();
                                w.set_thread_reply_count(count + 1);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to send thread reply: {}", e);
                    }
                }
            });
        });
    });
}

fn wire_load_more_thread_replies(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();
    let message_repo = ctx.message_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();

    window.on_load_more_thread_replies(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_repo = message_repo.clone();
        let user_repo = user_repo.clone();
        let attachment_repo = attachment_repo.clone();

        tokio::spawn(async move {
            let thread = match chat_state.active_thread() {
                Some(t) => t,
                None => {
                    tracing::warn!("Cannot load more thread replies: no active thread");
                    return;
                }
            };

            tracing::debug!(
                "Loading more replies for thread: {} in channel: {}",
                thread.root_message_id,
                thread.channel_id
            );

            // Load replies from local storage
            let replies = match message_repo.list_for_thread(&thread.root_message_id, None, 50)
            {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to load thread replies: {}", e);
                    return;
                }
            };

            // Convert to display data
            let mut display_messages: Vec<MessageDisplayData> = Vec::with_capacity(replies.len());
            for msg in &replies {
                let sender_name = if let Some(sender_id) = &msg.sender_id {
                    user_repo
                        .get_profile(sender_id)
                        .ok()
                        .flatten()
                        .map(|p| p.display_name)
                        .or_else(|| user_repo.get(sender_id).ok().flatten().map(|u| u.username))
                        .unwrap_or_else(|| "Unknown".to_string())
                } else {
                    "Anonymous".to_string()
                };

                let resolved_attachments: Vec<_> = msg
                    .attachments
                    .iter()
                    .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                    .collect();

                display_messages.push(message_to_display_data(msg, &sender_name, &resolved_attachments));
            }

            let reply_count = replies.len() as i32;

            // Update UI
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let slint_messages: Vec<_> =
                        display_messages.iter().map(|d| d.to_slint()).collect();
                    w.set_thread_replies(ModelRc::new(VecModel::from(slint_messages)));
                    tracing::debug!("Loaded {} thread replies", reply_count);
                }
            });
        });
    });
}

/// Load thread data when a thread is opened.
///
/// Called from chat_actions.rs when thread-clicked is triggered.
pub fn load_thread_data(
    window: slint::Weak<MainWindow>,
    _chat_state: ChatState,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
    attachment_repo: Arc<AttachmentRepository>,
    _channel_id: ChannelId,
    root_message_id: MessageId,
) {
    tokio::spawn(async move {
        // Get root message info
        let root_message = match message_repo.get(&root_message_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::warn!("Thread root message not found: {}", root_message_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to get thread root message: {}", e);
                return;
            }
        };

        // Get sender name
        let sender_name = if let Some(sender_id) = &root_message.sender_id {
            user_repo
                .get_profile(sender_id)
                .ok()
                .flatten()
                .map(|p| p.display_name)
                .or_else(|| user_repo.get(sender_id).ok().flatten().map(|u| u.username))
                .unwrap_or_else(|| "Unknown".to_string())
        } else {
            "Anonymous".to_string()
        };

        // Get content preview
        let content_preview = root_message
            .as_text()
            .map(|t| {
                if t.len() > 100 {
                    format!("{}...", &t[..97])
                } else {
                    t.to_string()
                }
            })
            .unwrap_or_else(|| "[Attachment]".to_string());

        // Load replies
        let replies = match message_repo.list_for_thread(&root_message_id, None, 50) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!("Failed to load thread replies: {}", e);
                Vec::new()
            }
        };

        let reply_count = replies.len() as i32;

        // Convert replies to display data
        let mut display_replies: Vec<MessageDisplayData> = Vec::with_capacity(replies.len());
        for msg in &replies {
            let reply_sender_name = if let Some(sender_id) = &msg.sender_id {
                user_repo
                    .get_profile(sender_id)
                    .ok()
                    .flatten()
                    .map(|p| p.display_name)
                    .or_else(|| user_repo.get(sender_id).ok().flatten().map(|u| u.username))
                    .unwrap_or_else(|| "Unknown".to_string())
            } else {
                "Anonymous".to_string()
            };

            let resolved_attachments: Vec<_> = msg
                .attachments
                .iter()
                .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
                .collect();

            display_replies.push(message_to_display_data(msg, &reply_sender_name, &resolved_attachments));
        }

        // Update UI
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window.upgrade() {
                w.set_thread_panel_visible(true);
                w.set_thread_root_message_id(root_message_id.to_string().into());
                w.set_thread_root_sender_name(sender_name.into());
                w.set_thread_root_content_preview(content_preview.into());
                w.set_thread_reply_count(reply_count);
                w.set_thread_channel_name(SharedString::default()); // TODO: get channel name
                w.set_thread_loading(false);

                let slint_replies: Vec<_> = display_replies.iter().map(|d| d.to_slint()).collect();
                w.set_thread_replies(ModelRc::new(VecModel::from(slint_replies)));

                tracing::info!(
                    "Thread panel opened for message {} with {} replies",
                    root_message_id,
                    reply_count
                );
            }
        });
    });
}
