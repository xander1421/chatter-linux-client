//! Message send/fail, channel member, pin/unpin, and unread count handlers.

use std::sync::Arc;

use slint::Weak;

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::{ChannelId, UserId};
use crate::MainWindow;

/// Handle unread count updated event.
pub(super) fn handle_unread_count_updated(
    window: &Weak<MainWindow>,
    channel_id: ChannelId,
    peer_id: Option<UserId>,
    unread_count: u32,
) {
    let weak = window.clone();
    let channel_id_str = channel_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Update sidebar unread badge
            super::helpers::update_unread_count(&w, &channel_id_str, unread_count);
        }
    });
    tracing::debug!(
        channel_id = %channel_id,
        ?peer_id,
        unread_count,
        "Updated unread count in UI"
    );
}

/// Handle DM bootstrap completed event.
pub(super) fn handle_dm_bootstrap_completed(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    peer_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let peer_id_str = peer_id.to_string();

    // Flush any pending messages for this peer
    let dm_bootstrap = ui_ctx.dm_bootstrap_service.clone();
    let pending = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            dm_bootstrap.take_pending_messages(&peer_id).await
        })
    });

    if !pending.is_empty() {
        tracing::info!(
            peer_id = %peer_id,
            pending_count = pending.len(),
            "Flushing pending messages after bootstrap"
        );

        let mailbox = ui_ctx.mailbox_manager.clone();
        let msg_svc = ui_ctx.message_service.clone();
        tokio::spawn(async move {
            for text in pending {
                let now = chrono::Utc::now().timestamp_millis() as u64;
                let content = chatter_linux::services::build_text_content(&text, now);
                if let Err(e) = msg_svc.send_dm(&peer_id, content, &*mailbox).await {
                    tracing::warn!(error = %e, "Failed to send pending message");
                }
            }
        });
    }

    // Update UI to enable composer
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(_w) = weak.upgrade() {
            // If we're in a DM with this peer, enable the composer
            // For now just log - we need to implement proper UI updates
            tracing::debug!(peer_id = %peer_id_str, "DM bootstrap complete - composer should be enabled");
            // This should trigger a UI update to enable the composer for this DM
        }
    });
}

/// Handle message sent event.
pub(super) fn handle_message_sent() {
    tracing::debug!("Message sent successfully");
    // UI updates handled by message state
}

/// Handle message send failed event.
pub(super) fn handle_message_send_failed(channel_id: ChannelId, error: String) {
    tracing::error!(
        channel_id = %channel_id,
        error = %error,
        "Message send failed"
    );
    // TODO: Show error notification in UI
}

/// Handle channel member joined event.
pub(super) fn handle_channel_member_joined(
    channel_id: ChannelId,
    user_id: UserId,
    display_name: String,
) {
    tracing::info!(
        channel_id = %channel_id,
        user_id = %user_id,
        display_name = %display_name,
        "Channel member joined"
    );
    // System message is already added to message state by EventHandlers
    // UI will update automatically through message state observation
}

/// Handle channel member left event.
pub(super) fn handle_channel_member_left(
    channel_id: ChannelId,
    user_id: UserId,
    display_name: String,
) {
    tracing::info!(
        channel_id = %channel_id,
        user_id = %user_id,
        display_name = %display_name,
        "Channel member left"
    );
    // System message is already added to message state by EventHandlers
    // UI will update automatically through message state observation
}
