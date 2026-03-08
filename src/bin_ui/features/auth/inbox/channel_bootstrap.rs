//! Channel bootstrap pending message flush handler.
//!
//! When a new member's mailbox address is received via `GroupMailboxReceived`,
//! this handler flushes any pending channel messages that were queued during
//! the bootstrap phase when mailboxes weren't available yet.

use std::sync::Arc;

use chatter_linux::app::UIContext;
use chatter_linux::domain::{ChannelId, UserId};
use chatter_linux::inbox::DomainEvent;
use chatter_linux::mailbox::group::GroupFanout;
use chatter_linux::proto::mailbox_v1::EnvelopeType;

/// Wire the channel bootstrap handler to process incoming mailbox announcements.
///
/// Subscribes to `DomainEvent::GroupMailboxReceived` and flushes pending messages
/// that were queued during channel bootstrap when mailboxes weren't available.
pub fn wire_channel_bootstrap_handler(ui_ctx: &Arc<UIContext>) {
    let grpc_pool = ui_ctx.grpc_pool.clone();
    let signal_service = ui_ctx.signal_service.clone();
    let group_fanout = ui_ctx.group_fanout.clone();
    let group_mailbox_manager = ui_ctx.group_mailbox_manager.clone();
    let mut event_rx = ui_ctx.event_bus.subscribe_domain();
    let runtime = ui_ctx.runtime_handle.clone();

    // Spawn a task to handle domain events
    runtime.spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            if let DomainEvent::GroupMailboxReceived {
                sender_id,
                channel_id,
                ..
            } = event
            {
                // Get the group fanout if available
                let fanout = match group_fanout.get() {
                    Some(f) => f.clone(),
                    None => {
                        tracing::debug!(
                            channel_id = %channel_id,
                            "GroupFanout not initialized yet - skipping pending message flush"
                        );
                        continue;
                    }
                };

                // Check for pending messages using nil member (broadcast queue)
                let nil_member = UserId::from_uuid(uuid::Uuid::nil());
                let pending_messages = match group_mailbox_manager.drain_pending_messages(
                    &channel_id,
                    &nil_member,
                ) {
                    Ok(msgs) => msgs,
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel_id,
                            error = %e,
                            "Failed to drain pending channel messages"
                        );
                        continue;
                    }
                };

                if pending_messages.is_empty() {
                    tracing::debug!(
                        channel_id = %channel_id,
                        sender_id = %sender_id,
                        "No pending messages to flush for channel"
                    );
                    continue;
                }

                tracing::info!(
                    channel_id = %channel_id,
                    sender_id = %sender_id,
                    count = pending_messages.len(),
                    "[BOOTSTRAP] Flushing {} pending message(s) to new member",
                    pending_messages.len()
                );

                // Flush each pending message
                let pool = grpc_pool.clone();
                let signal = signal_service.clone();
                let fanout_clone = fanout.clone();
                let channel_id_clone = channel_id;
                let sender_id_clone = sender_id;

                tokio::spawn(async move {
                    if let Err(e) = flush_pending_messages(
                        &pool,
                        &signal,
                        &fanout_clone,
                        &channel_id_clone,
                        &sender_id_clone,
                        pending_messages,
                    )
                    .await
                    {
                        tracing::error!(
                            channel_id = %channel_id_clone,
                            error = %e,
                            "[BOOTSTRAP] Failed to flush pending messages"
                        );
                    }
                });
            }
        }
    });
}

/// Flush pending messages to a specific member.
async fn flush_pending_messages(
    grpc_pool: &Arc<chatter_linux::network::grpc::client_pool::GrpcClientPool>,
    signal_service: &Arc<chatter_linux::services::signal_service::SignalService>,
    group_fanout: &Arc<GroupFanout>,
    channel_id: &ChannelId,
    member_id: &UserId,
    pending_messages: Vec<(Vec<u8>, u64)>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::collections::HashMap;

    let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
        grpc_pool.channel(),
        grpc_pool.auth_clone(),
    );

    // Get the member's mailbox from group state
    let group_state = group_fanout
        .get_group_state(channel_id)?
        .ok_or("Group state not found")?;

    // Verify member has a mailbox (we don't use it directly since send_to_group_pairwise handles lookup)
    if !group_state.member_mailboxes.contains_key(member_id) {
        return Err(format!("Member mailbox not found for {}", member_id).into());
    }

    for (plaintext, timestamp) in pending_messages {
        tracing::debug!(
            channel_id = %channel_id,
            member_id = %member_id,
            plaintext_len = plaintext.len(),
            timestamp = timestamp,
            "[BOOTSTRAP] Sending pending message"
        );

        // Encrypt pairwise for the member using sealed sender
        let sealed_ciphertext = match signal_service
            .encrypt_message(
                &mut auth_client,
                &member_id.to_string(),
                1, // Default to primary device
                &plaintext,
            )
            .await
        {
            Ok(ct) => ct,
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    member_id = %member_id,
                    error = %e,
                    "[BOOTSTRAP] Failed to encrypt pending message"
                );
                continue;
            }
        };

        // Send to the member's mailbox
        let mut member_ciphertexts: HashMap<UserId, Vec<u8>> = HashMap::new();
        member_ciphertexts.insert(*member_id, sealed_ciphertext);

        match group_fanout
            .send_to_group_pairwise(channel_id, member_ciphertexts, EnvelopeType::Message)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    channel_id = %channel_id,
                    member_id = %member_id,
                    successful = result.successful,
                    failed = result.failed,
                    "[BOOTSTRAP] Pending message sent to new member"
                );
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    member_id = %member_id,
                    error = %e,
                    "[BOOTSTRAP] Failed to send pending message to mailbox"
                );
            }
        }
    }

    Ok(())
}
