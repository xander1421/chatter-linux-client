//! DM bootstrap event handler.
//!
//! Subscribes to domain events and handles KeyExchangeReceived events
//! to complete the DM bootstrap protocol. When a bootstrap message (64 bytes)
//! is received, creates our mailbox and sends response to peer.
//!
//! # Protocol (NDSS '21)
//!
//! The response uses two encryption layers:
//! 1. **Content Layer**: Signal Protocol (X3DH/Double Ratchet) with peer's identity
//! 2. **Transport Layer**: Envelope encryption using ECDH(our_identity, peer_mailbox_ephemeral)
//!
//! The transport layer is stateless and ensures only the mailbox owner can decrypt.
//!
//! # Security
//!
//! This handler receives a shared `Arc<Database>` rather than reopening the database.
//! This means the database passphrase is never passed around after initial unlock.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::crypto::envelope_crypto::encrypt_envelope;
use crate::domain::UserId;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::dm::{release_bootstrap_lock, BootstrapResponse, DmBootstrap};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::AnonymousMailboxClient;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::message::{queued_bubble_timestamp, PendingFlush};
use crate::services::SignalService;
use crate::storage::{Database, KeyStore, SettingsRepository};
use crate::util::time::TimestampNanos;

/// Spawn the DM bootstrap event handler from EventBus.
///
/// This is the preferred way to spawn the handler - it subscribes to the
/// domain event channel from the EventBus.
///
/// # Arguments
///
/// * `database` - Shared database connection (already unlocked)
/// * `grpc_pool` - Shared gRPC connection pool
/// * `event_bus` - EventBus to subscribe for domain events
///
/// # Security
///
/// The database passphrase is NOT passed to this function. The database
/// must be unlocked before calling this function. This ensures the passphrase
/// can be zeroized immediately after database initialization.
pub fn spawn_dm_bootstrap_handler_from_bus(
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    event_bus: &EventBus,
) -> JoinHandle<()> {
    let event_rx = event_bus.subscribe_domain();
    spawn_dm_bootstrap_handler_with_receiver(database, grpc_pool, event_rx, event_bus.clone())
}

/// Spawn the DM bootstrap event handler with a custom receiver.
///
/// Signal operations are handled asynchronously via the SignalActor.
pub fn spawn_dm_bootstrap_handler_with_receiver(
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    mut event_rx: broadcast::Receiver<DomainEvent>,
    event_bus: EventBus,
) -> JoinHandle<()> {
    // Track in-progress bootstraps to prevent concurrent requests for same peer
    let in_progress: Arc<Mutex<HashSet<UserId>>> = Arc::new(Mutex::new(HashSet::new()));

    // Get user ID from grpc_pool for UUID comparison
    let auth_user_id = grpc_pool.auth_info().user_id.clone();

    tokio::spawn(async move {
        tracing::info!("DM bootstrap handler started");

        // Per-bootstrap tasks are owned by this JoinSet. When the handler
        // exits (channel closed) or is aborted, the JoinSet drops and every
        // in-flight bootstrap task is aborted with it — no detached writes
        // continuing past the handler's lifetime.
        let mut bootstrap_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                // Drain finished tasks so the JoinSet doesn't grow unbounded.
                // The `if !is_empty()` guard disables this branch when the set
                // is empty (otherwise `join_next` returns `None` immediately
                // and busy-loops select!).
                Some(res) = bootstrap_tasks.join_next(), if !bootstrap_tasks.is_empty() => {
                    if let Err(e) = res {
                        if e.is_panic() {
                            tracing::error!(error = %e, "DM bootstrap task panicked");
                        }
                    }
                },
                event = event_rx.recv() => match event {
                    Ok(DomainEvent::KeyExchangeReceived {
                        sender_id,
                        sender_device_id,
                        conversation_id: _,
                        ephemeral_public_key,
                        timestamp: _,
                    }) => {
                        // Check key length to determine message type:
                        // - 64 bytes = bootstrap message (mailbox + conversation key)
                        // - 0 bytes = bootstrap REQUEST (peer wants us to initiate)
                        // - 32 bytes = regular key exchange response
                        if ephemeral_public_key.len() == 64 {
                        // Check if bootstrap is already in progress for this peer
                        {
                            let mut guard = in_progress.lock().await;
                            if guard.contains(&sender_id) {
                                tracing::debug!(
                                    peer = %sender_id,
                                    "BOOTSTRAP[3/4] Skipping - already in progress for this peer"
                                );
                                continue;
                            }
                            guard.insert(sender_id);
                        }

                        tracing::info!(
                            peer = %sender_id,
                            key_len = ephemeral_public_key.len(),
                            "BOOTSTRAP[3/4] Received 64-byte KeyExchange = bootstrap MESSAGE from peer"
                        );

                        let database = database.clone();
                        let grpc_pool = grpc_pool.clone();
                        let ephemeral_key = ephemeral_public_key.clone();
                        let in_progress_clone = in_progress.clone();
                        let event_bus = event_bus.clone();

                        bootstrap_tasks.spawn(async move {
                            tracing::debug!(
                                peer = %sender_id,
                                "BOOTSTRAP[3/4] handler task started"
                            );

                            let result = handle_bootstrap_async(
                                database.clone(),
                                grpc_pool.clone(),
                                sender_id,
                                sender_device_id,
                                &ephemeral_key,
                                &event_bus,
                            )
                            .await;

                            // Release the global bootstrap lock for this peer.
                            // This lock was held by the higher UUID user when they sent the REQUEST.
                            // Now that we've processed their MESSAGE, they can send actual messages.
                            release_bootstrap_lock(&sender_id);
                            tracing::debug!(
                                peer = %sender_id,
                                "BOOTSTRAP[3/4] Released global bootstrap lock for peer"
                            );

                            if let Err(e) = result {
                                tracing::error!(
                                    peer = %sender_id,
                                    error = %e,
                                    "Failed to handle DM bootstrap"
                                );
                            }

                            // Remove from in-progress set when done
                            in_progress_clone.lock().await.remove(&sender_id);

                            tracing::debug!(
                                peer = %sender_id,
                                "BOOTSTRAP[3/4] handler task completed"
                            );
                        });
                    } else if ephemeral_public_key.is_empty() {
                        // Bootstrap REQUEST: peer (higher UUID) wants us to initiate
                        // Verify we are actually the lower UUID before initiating

                        // Check if bootstrap is already in progress for this peer
                        {
                            let mut guard = in_progress.lock().await;
                            if guard.contains(&sender_id) {
                                tracing::debug!(
                                    peer = %sender_id,
                                    "BOOTSTRAP[2/4] Skipping REQUEST - already in progress for this peer"
                                );
                                continue;
                            }
                            guard.insert(sender_id);
                        }

                        tracing::info!(
                            peer = %sender_id,
                            our_id = %auth_user_id,
                            key_len = ephemeral_public_key.len(),
                            "BOOTSTRAP[2/4] Received 0-byte KeyExchange = bootstrap REQUEST from peer"
                        );

                        let our_uuid = match auth_user_id.parse::<uuid::Uuid>() {
                            Ok(u) => u,
                            Err(_) => {
                                tracing::error!(
                                    peer = %sender_id,
                                    "BOOTSTRAP[2/4] FAILED - Invalid local user UUID"
                                );
                                // Remove from in-progress on early exit
                                in_progress.lock().await.remove(&sender_id);
                                continue;
                            }
                        };

                        let peer_uuid = sender_id.as_uuid();

                        tracing::debug!(
                            our_uuid = %our_uuid,
                            peer_uuid = %peer_uuid,
                            we_are_lower = our_uuid < *peer_uuid,
                            "BOOTSTRAP[2/4] Checking UUID order for deterministic initiator"
                        );

                        if our_uuid < *peer_uuid {
                            // We ARE the lower UUID - we should initiate
                            tracing::info!(
                                peer = %sender_id,
                                our_id = %auth_user_id,
                                "BOOTSTRAP[2/4] We're lower UUID - will initiate bootstrap MESSAGE"
                            );

                            let database = database.clone();
                            let grpc_pool = grpc_pool.clone();
                            let in_progress_clone = in_progress.clone();

                            bootstrap_tasks.spawn(async move {
                                if let Err(e) = handle_bootstrap_request_async(
                                    database,
                                    grpc_pool,
                                    sender_id,
                                    sender_device_id,
                                )
                                .await
                                {
                                    tracing::error!(
                                        peer = %sender_id,
                                        error = %e,
                                        "Failed to handle bootstrap request"
                                    );
                                }
                                // Remove from in-progress set when done
                                in_progress_clone.lock().await.remove(&sender_id);
                            });
                        } else {
                            // We're the higher UUID - ignore (we shouldn't receive this)
                            tracing::warn!(
                                peer = %sender_id,
                                our_id = %auth_user_id,
                                our_uuid = %our_uuid,
                                peer_uuid = %peer_uuid,
                                "BOOTSTRAP[2/4] IGNORED - We're higher UUID, shouldn't receive REQUEST"
                            );
                            // Remove from in-progress since we're not handling it
                            in_progress.lock().await.remove(&sender_id);
                        }
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
        }
    })
}

/// Handle bootstrap REQUEST asynchronously.
///
/// When higher UUID peer requests bootstrap, we (lower UUID) initiate:
/// 1. Check if session already exists or is pending
/// 2. If not, create our mailbox and send bootstrap message to peer
///
/// Uses shared database and gRPC pool - no passphrase needed.
async fn handle_bootstrap_request_async(
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    peer_id: UserId,
    // The device that sent the REQUEST — the one waiting for our MESSAGE.
    peer_device_id: u32,
) -> Result<(), String> {
    use crate::mailbox::dm::DmConversationManager;
    use crate::mailbox::TokenManager;
    use crate::proto::chat_v1::SealedSenderMessage;

    // Check if we already have a session or pending bootstrap
    let conversation_manager = DmConversationManager::new(database.clone());
    let existing = conversation_manager
        .load_conversation(&peer_id)
        .map_err(|e| format!("Failed to load conversation: {}", e))?;

    if let Some(ref conv) = existing {
        if !conv.needs_bootstrap() {
            tracing::info!(
                peer = %peer_id,
                "Bootstrap request received but session already exists - ignoring"
            );
            return Ok(());
        }

        // Check if bootstrap is already in progress (we have our mailbox)
        if conv.our_mailbox.is_some() {
            tracing::info!(
                peer = %peer_id,
                "Bootstrap request received but we already initiated - waiting for response"
            );
            return Ok(());
        }
    }

    tracing::info!(
        peer = %peer_id,
        "BOOTSTRAP[2/4] Processing REQUEST - initiating as lower UUID"
    );

    // Create fresh gRPC channel
    // Get channel and auth interceptor from pool
    tracing::debug!(
        peer = %peer_id,
        "BOOTSTRAP[2/4] Getting gRPC channel for bootstrap initiation"
    );
    let channel = grpc_pool.channel();
    let auth_interceptor = grpc_pool.auth_clone();

    // Create DmBootstrap and initiate
    let dm_bootstrap = DmBootstrap::new(database.clone());
    let mut anon_mailbox_client = AnonymousMailboxClient::new(channel.clone());

    let bootstrap_result = dm_bootstrap
        .initiate_dm(peer_id, &mut anon_mailbox_client)
        .await
        .map_err(|e| format!("Failed to initiate bootstrap: {}", e))?;

    tracing::info!(
        peer = %peer_id,
        mailbox_id = %bootstrap_result.our_mailbox.id,
        bootstrap_msg_len = bootstrap_result.bootstrap_message.len(),
        "BOOTSTRAP[2/4] Created mailbox, now encrypting 64-byte MESSAGE for peer"
    );

    // Encrypt bootstrap message with Signal Protocol
    let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
    let key_store = Arc::new(KeyStore::new(database.clone()));
    let signal_service = SignalService::new(key_store, settings_repo);

    let mut auth_client = AuthClient::new(channel.clone(), auth_interceptor.clone());
    let peer_id_str = peer_id.to_string();

    let sealed_ciphertext = signal_service
        .encrypt_message(&mut auth_client, &peer_id_str, peer_device_id, &bootstrap_result.bootstrap_message)
        .await
        .map_err(|e| format!("Failed to encrypt bootstrap: {}", e))?;

    // Spend a pre-fetched token; never request-then-spend (NDSS'21 §VI-F).
    let token = TokenManager::new(database.clone())
        .claim_prefetched_token()
        .map_err(|e| format!("No anonymous token for the bootstrap MESSAGE: {e}"))?;

    // Send to peer's device inbox via anonymous client (no AuthInterceptor)
    // to avoid dual-auth rejection — GRA-436
    let mut anon_client = grpc_pool.anonymous_message_client();

    let sealed_msg = SealedSenderMessage {
        sealed_body: sealed_ciphertext,
        server_key_id: signal_service.get_server_key_id(),
        timestamp: TimestampNanos::now().as_u64(),
    };

    anon_client
        .send_with_anonymous_credential(
            &peer_id_str,
            peer_device_id,
            sealed_msg,
            &token.public_key,
            &token.signature,
            token.randomizer.as_deref(),
        )
        .await
        .map_err(|e| format!("Failed to send bootstrap: {}", e))?;

    tracing::info!(
        peer = %peer_id,
        "BOOTSTRAP[2/4] SUCCESS - 64-byte MESSAGE sent to peer's device inbox"
    );
    tracing::info!(
        peer = %peer_id,
        "BOOTSTRAP[2/4] Now waiting for peer to send RESPONSE to our mailbox"
    );

    Ok(())
}

/// Handle bootstrap asynchronously with shared resources.
///
/// Uses shared database and gRPC pool - no passphrase needed.
async fn handle_bootstrap_async(
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    peer_id: UserId,
    // The device that sent the bootstrap MESSAGE — the one our RESPONSE is for.
    peer_device_id: u32,
    ephemeral_public_key: &[u8],
    event_bus: &EventBus,
) -> Result<(), String> {
    tracing::info!(
        peer = %peer_id,
        key_len = ephemeral_public_key.len(),
        "BOOTSTRAP[3/4] Processing 64-byte MESSAGE - will send RESPONSE"
    );

    // Get channel and auth interceptor from pool
    tracing::debug!(
        peer = %peer_id,
        "BOOTSTRAP[3/4] Getting gRPC channel"
    );
    let channel = grpc_pool.channel();
    let auth_interceptor = grpc_pool.auth_clone();
    tracing::debug!(peer = %peer_id, "BOOTSTRAP[3/4] gRPC channel obtained");

    // Create DmBootstrap
    tracing::info!(peer = %peer_id, "BOOTSTRAP[3/4] Creating DmBootstrap");
    let dm_bootstrap = DmBootstrap::new(database.clone());

    // Anonymous mailbox client for mailbox creation (tokens come from the
    // pre-fetched batch, never requested here)
    let mut anon_mailbox_client = AnonymousMailboxClient::new(channel.clone());

    // Release the bootstrap lock that was held since sending REQUEST
    // (Higher UUID holds lock while waiting for MESSAGE - now release it for processing)
    release_bootstrap_lock(&peer_id);
    tracing::debug!(peer = %peer_id, "BOOTSTRAP[3/4] Released REQUEST lock before processing MESSAGE");

    // Process the incoming bootstrap
    tracing::info!(peer = %peer_id, "BOOTSTRAP[3/4] Calling handle_incoming_bootstrap");
    let response = match dm_bootstrap
        .handle_incoming_bootstrap(peer_id, ephemeral_public_key, &mut anon_mailbox_client)
        .await
    {
        Ok(Some(resp)) => {
            tracing::info!(peer = %peer_id, "BOOTSTRAP[3/4] handle_incoming_bootstrap returned response");
            resp
        }
        Ok(None) => {
            tracing::info!(peer = %peer_id, "BOOTSTRAP[3/4] handle_incoming_bootstrap returned None");
            return Ok(());
        }
        Err(e) => {
            tracing::error!(peer = %peer_id, error = %e, "BOOTSTRAP[3/4] handle_incoming_bootstrap failed");
            return Err(format!("Failed to handle bootstrap: {}", e));
        }
    };

    // Create a SINGLE SignalService instance for all encryptions in this bootstrap flow.
    // This is critical - using separate instances can cause session counter desync.
    let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
    let key_store = Arc::new(KeyStore::new(database.clone()));
    let signal_service = SignalService::new(key_store.clone(), settings_repo.clone());
    let mut auth_client = AuthClient::new(channel.clone(), auth_interceptor.clone());

    // Send bootstrap response with two-layer encryption
    let our_device_id = settings_repo
        .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
        .map_err(|e| format!("Failed to read our device id: {e}"))?
        .ok_or("No current device id")?;
    send_bootstrap_response_with_envelope(
        response,
        peer_device_id,
        our_device_id,
        &signal_service,
        &mut auth_client,
        channel.clone(),
    )
    .await?;

    // Bootstrap complete for responder (higher UUID) - send any pending messages
    tracing::info!(
        peer = %peer_id,
        "BOOTSTRAP[3/4] SUCCESS - RESPONSE sent to peer's DM mailbox"
    );
    tracing::info!(
        peer = %peer_id,
        "BOOTSTRAP[3/4] Now draining and sending pending messages (if any)"
    );

    // The queue was drained, so a failed send is otherwise a message lost
    // without a word: announce it like the initiator-side flush does.
    let flush = send_pending_messages_after_bootstrap(
        peer_id,
        peer_device_id,
        database,
        &signal_service,
        &mut auth_client,
        channel,
    )
    .await;
    if let Some(flush) = flush {
        flush.emit(peer_id, event_bus);
    }

    Ok(())
}

/// Send pending messages after bootstrap completes (responder side).
///
/// This is called after the higher UUID user responds to a bootstrap request.
/// At this point, the shared secret is established and queued messages can be sent.
/// `None` when nothing was queued; otherwise the outcome of every message.
///
/// IMPORTANT: Uses the same SignalService instance as the bootstrap response
/// to ensure session counters stay in sync.
async fn send_pending_messages_after_bootstrap(
    peer_id: UserId,
    peer_device_id: u32,
    database: Arc<Database>,
    signal_service: &SignalService,
    auth_client: &mut AuthClient,
    channel: crate::network::grpc::transport::GrpcTransport,
) -> Option<PendingFlush> {
    use prost::Message as ProstMessage;
    use crate::crypto::signal::sesame::{SesameManager, SqliteSesameStore};
    use crate::mailbox::dm::DmConversationManager;

    let sesame_manager = SesameManager::new(SqliteSesameStore::new(database.clone()));
    let pending = match sesame_manager.drain_pending_messages(&peer_id) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!(peer = %peer_id, error = %e, "BOOTSTRAP[3/4] Failed to drain pending messages");
            return None;
        }
    };
    if pending.is_empty() {
        tracing::info!(peer = %peer_id, "BOOTSTRAP[3/4] No pending messages queued - nothing to send");
        return None;
    }
    tracing::info!(peer = %peer_id, count = pending.len(), "BOOTSTRAP[3/4] Sending pending messages (responder side)");

    // From here the queue is drained: every failure loses messages, so each
    // one is reported to the caller rather than only logged.
    let queued: Vec<_> = pending
        .into_iter()
        .map(|msg| {
            let bubble = crate::proto::content_v1::Content::decode(msg.content.as_slice())
                .ok()
                .as_ref()
                .and_then(queued_bubble_timestamp);
            (bubble, msg.content)
        })
        .collect();
    let mut flush = PendingFlush::default();

    let conversation = DmConversationManager::new(database.clone()).load_conversation(&peer_id);
    let (send_key, peer_mailbox) = match conversation {
        Ok(Some(conv)) => match (conv.conversation_key.as_ref().and_then(|k| k.send_key()), conv.peer_mailbox) {
            (Some(key), Some(mailbox)) => (key, mailbox),
            _ => {
                for (bubble, _) in queued {
                    flush.record(bubble, Err("no mailbox keys after bootstrap".into()));
                }
                return Some(flush);
            }
        },
        Ok(None) => {
            for (bubble, _) in queued {
                flush.record(bubble, Err("no conversation state after bootstrap".into()));
            }
            return Some(flush);
        }
        Err(e) => {
            for (bubble, _) in queued {
                flush.record(bubble, Err(e.to_string()));
            }
            return Some(flush);
        }
    };

    let mut anon_mailbox_client = AnonymousMailboxClient::new(channel);
    for (bubble, content) in queued {
        let result = async {
            for envelope in seal_for_peer_mailbox(signal_service, auth_client, &peer_id, peer_device_id, &content, &send_key).await? {
                anon_mailbox_client
                    .send_to_ephemeral_mailbox(peer_mailbox.public_key.to_vec(), envelope, EnvelopeType::Message as i32)
                    .await
                    .map_err(|e| format!("mailbox send: {e}"))?;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(e) = &result {
            tracing::error!(peer = %peer_id, error = %e, "Failed to send pending message");
        }
        flush.record(bubble, result);
    }

    tracing::info!(
        peer = %peer_id,
        sent = flush.sent.len(),
        failed = flush.failed.len(),
        "Finished sending pending messages after bootstrap"
    );
    Some(flush)
}

/// Seal `plaintext` for `peer_device_id` of `peer_id`, wrapped for the peer's
/// mailbox under `send_key`.
///
/// Only that device: it sent the bootstrap MESSAGE, so it owns the mailbox
/// this conversation delivers to (the mailbox conversation is
/// one-peer-device) and has just left a session (the RESPONSE was encrypted
/// under it). Sealing to every session device also sealed to retired
/// installs that no longer exist. Never a prekey fetch: this flush runs in
/// Shield, where naming the peer in an identified RPC is the link the mailbox
/// exists to hide.
pub async fn seal_for_peer_mailbox(
    signal_service: &SignalService,
    auth_client: &mut AuthClient,
    peer_id: &UserId,
    peer_device_id: u32,
    plaintext: &[u8],
    send_key: &[u8; 32],
) -> Result<Vec<Vec<u8>>, String> {
    signal_service
        .encrypt_message_session_devices(auth_client, &peer_id.to_string(), vec![peer_device_id], plaintext)
        .await
        .map_err(|e| format!("Signal encrypt: {e}"))?
        .into_values()
        .map(|ciphertext| {
            crate::crypto::dm_crypto::encrypt_dm_bytes(&ciphertext, send_key)
                .map_err(|e| format!("mailbox encrypt: {e}"))
        })
        .collect()
}

/// Send bootstrap response with proper two-layer encryption.
///
/// Layer 1 (Content): Signal Protocol encryption to peer's identity
/// Layer 2 (Transport): Envelope encryption using ECDH(fresh_ephemeral, peer_mailbox)
///
/// IMPORTANT: Uses the shared SignalService instance to ensure session counters
/// stay in sync with pending message sending.
async fn send_bootstrap_response_with_envelope(
    response: BootstrapResponse,
    peer_device_id: u32,
    our_device_id: u32,
    signal_service: &SignalService,
    auth_client: &mut AuthClient,
    channel: crate::network::grpc::transport::GrpcTransport,
) -> Result<(), String> {
    let peer_id = response.peer_id;

    // Build the response message (KeyExchange proto)
    let response_message = response
        .build_response_message()
        .map_err(|e| format!("Failed to build response message: {}", e))?;

    // Layer 1: Encrypt content with Signal Protocol (using shared signal_service)
    let peer_id_str = peer_id.to_string();

    let (signal_ciphertext, msg_type) = signal_service
        .encrypt_message_identified(auth_client, &peer_id_str, peer_device_id, &response_message)
        .await
        .map_err(|e| format!("Failed to encrypt with Signal: {}", e))?;

    tracing::debug!(
        peer = %peer_id,
        ciphertext_len = signal_ciphertext.len(),
        msg_type,
        "Encrypted bootstrap response content with Signal Protocol"
    );

    // Layer 2: Wrap in an envelope encrypted with ECDH(fresh_ephemeral, peer_mailbox).
    // The envelope carries NO long-term key: its 32-byte prefix is a per-message
    // ephemeral public key, so the unauthenticated deposit below does not name us.
    // The initiator can only decrypt the Signal layer against OUR device's
    // address, so it rides in the frame, inside the envelope.
    use prost::Message as _;
    let frame = crate::proto::content_v1::BootstrapResponseFrame {
        sender_device_id: our_device_id,
        signal_ciphertext,
    }
    .encode_to_vec();
    let envelope = encrypt_envelope(&frame, &response.peer_mailbox_public_key)
        .map_err(|e| format!("Failed to encrypt envelope: {}", e))?;

    tracing::debug!(
        peer = %peer_id,
        envelope_len = envelope.len(),
        "Encrypted bootstrap response envelope"
    );

    // Send to peer's ephemeral mailbox (anonymous send, no auth required)
    // Uses anonymous client to avoid leaking device identity on anonymous endpoint
    let mut anon_client = AnonymousMailboxClient::new(channel);

    anon_client
        .send_to_ephemeral_mailbox(
            response.peer_mailbox_public_key.to_vec(),
            envelope,
            EnvelopeType::PrekeyMessage as i32,
        )
        .await
        .map_err(|e| format!("Failed to send to mailbox: {}", e))?;

    tracing::info!(
        peer = %peer_id,
        "Sent DM bootstrap response (Signal + envelope encrypted)"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
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
