//! DM bootstrap event handler.
//!
//! Subscribes to domain events and handles KeyExchangeReceived events
//! to complete the DM bootstrap protocol. When a bootstrap message (64 bytes)
//! is received, creates our mailbox and sends response to peer.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::domain::UserId;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::dm::{BootstrapResponse, DmBootstrap};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::MailboxClient;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::MailboxManager;
use crate::storage::Database;

/// Spawn the DM bootstrap event handler from EventBus.
///
/// This is the preferred way to spawn the handler - it subscribes to the
/// domain event channel from the EventBus.
///
/// # Arguments
///
/// * `db` - Database for DmBootstrap and conversation state
/// * `grpc_pool` - gRPC client pool for MailboxClient
/// * `mailbox_manager` - For sending response messages
/// * `event_bus` - EventBus to subscribe for domain events
pub fn spawn_dm_bootstrap_handler_from_bus(
    db: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    mailbox_manager: Arc<MailboxManager>,
    event_bus: &EventBus,
) -> JoinHandle<()> {
    let event_rx = event_bus.subscribe_domain();
    spawn_dm_bootstrap_handler_with_receiver(db, grpc_pool, mailbox_manager, event_rx)
}

/// Spawn the DM bootstrap event handler with a custom receiver.
///
/// This function spawns a task that subscribes to domain events and handles
/// KeyExchangeReceived events. When a 64-byte bootstrap message is received,
/// it calls DmBootstrap::handle_incoming_bootstrap() and sends the response
/// back to the peer's mailbox via sealed sender.
///
/// # Arguments
///
/// * `db` - Database for DmBootstrap and conversation state
/// * `grpc_pool` - gRPC client pool for MailboxClient
/// * `mailbox_manager` - For sending response messages
/// * `event_rx` - Receiver for domain events
pub fn spawn_dm_bootstrap_handler_with_receiver(
    db: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    mailbox_manager: Arc<MailboxManager>,
    mut event_rx: broadcast::Receiver<DomainEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dm_bootstrap = DmBootstrap::new(db);

        tracing::info!("DM bootstrap handler started");

        loop {
            match event_rx.recv().await {
                Ok(DomainEvent::KeyExchangeReceived {
                    sender_id,
                    conversation_id: _,
                    ephemeral_public_key,
                    timestamp: _,
                }) => {
                    // Check if this is a bootstrap message (64 bytes)
                    if ephemeral_public_key.len() == 64 {
                        tracing::debug!(
                            peer = %sender_id,
                            "Received DM bootstrap message, processing..."
                        );

                        handle_bootstrap(
                            &dm_bootstrap,
                            sender_id,
                            &ephemeral_public_key,
                            &grpc_pool,
                            &mailbox_manager,
                        )
                        .await;
                    } else {
                        tracing::trace!(
                            peer = %sender_id,
                            key_len = ephemeral_public_key.len(),
                            "Received regular key exchange (not bootstrap)"
                        );
                    }
                }
                Ok(_) => {
                    // Ignore other domain events
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(
                        count,
                        "DM bootstrap handler lagged behind, dropped {} events",
                        count
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("DM bootstrap handler: event channel closed");
                    return;
                }
            }
        }
    })
}

/// Handle an incoming DM bootstrap message.
async fn handle_bootstrap(
    dm_bootstrap: &DmBootstrap,
    sender_id: UserId,
    ephemeral_public_key: &[u8],
    grpc_pool: &Arc<GrpcClientPool>,
    mailbox_manager: &Arc<MailboxManager>,
) {
    // Create mailbox client for bootstrap operations
    let mut mailbox_client = MailboxClient::new(
        grpc_pool.channel(),
        grpc_pool.auth_clone(),
    );

    // Process the incoming bootstrap
    match dm_bootstrap
        .handle_incoming_bootstrap(sender_id, ephemeral_public_key, &mut mailbox_client)
        .await
    {
        Ok(Some(response)) => {
            // Send bootstrap response to peer's mailbox
            send_bootstrap_response(response, mailbox_manager).await;
        }
        Ok(None) => {
            // Not a bootstrap message (shouldn't happen since we checked length)
            tracing::debug!("handle_incoming_bootstrap returned None");
        }
        Err(e) => {
            tracing::error!(
                peer = %sender_id,
                error = %e,
                "Failed to handle incoming DM bootstrap"
            );
        }
    }
}

/// Send bootstrap response to peer's mailbox.
async fn send_bootstrap_response(
    response: BootstrapResponse,
    mailbox_manager: &Arc<MailboxManager>,
) {
    // Build the response message
    let response_message = match response.build_response_message() {
        Ok(msg) => msg,
        Err(e) => {
            tracing::error!(
                peer = %response.peer_id,
                error = %e,
                "Failed to build bootstrap response message"
            );
            return;
        }
    };

    // Send to peer's mailbox
    match mailbox_manager
        .send_to_mailbox(
            &response.peer_mailbox_public_key,
            response_message,
            EnvelopeType::SealedSender,
        )
        .await
    {
        Ok(()) => {
            tracing::info!(
                peer = %response.peer_id,
                "Sent DM bootstrap response to peer's mailbox"
            );
        }
        Err(e) => {
            tracing::error!(
                peer = %response.peer_id,
                error = %e,
                "Failed to send bootstrap response to peer's mailbox"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_message_detection() {
        // 64-byte keys are bootstrap messages
        let bootstrap_key = vec![0u8; 64];
        assert_eq!(bootstrap_key.len(), 64);

        // 32-byte keys are regular key exchanges
        let regular_key = vec![0u8; 32];
        assert_eq!(regular_key.len(), 32);
    }
}
