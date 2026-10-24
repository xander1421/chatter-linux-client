//! DM bootstrap event handler.
//!
//! Subscribes to domain events and handles KeyExchangeReceived events
//! to complete the DM bootstrap protocol. When a bootstrap message (a
//! `MailboxAnnouncement`) is received, creates our mailbox and sends response to peer.
//!
//! # Protocol (NDSS '21)
//!
//! The response uses two encryption layers:
//! 1. **Content Layer**: Signal Protocol (X3DH/Double Ratchet) with peer's identity
//! 2. **Transport Layer**: HPKE envelope sealed to the peer mailbox's X25519
//!    encryption key (fresh ephemeral per seal; never the mailbox signing key)
//!
//! The transport layer is stateless and ensures only the mailbox owner can decrypt.
//!
//! # Security
//!
//! This handler receives a shared `Arc<Database>` rather than reopening the database.
//! This means the database passphrase is never passed around after initial unlock.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use prost::Message as _;

use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::crypto::envelope_crypto::encrypt_envelope;
use crate::domain::UserId;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::dm::{BootstrapResponse, DmBootstrap, release_bootstrap_lock};
use crate::network::grpc::AnonymousMailboxClient;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::SignalService;
use crate::services::message::{MessageService, PendingFlush, queued_bubble_timestamp};
use crate::storage::repositories::PendingDmRepository;
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
    message_service: Arc<MessageService>,
    event_bus: &EventBus,
) -> JoinHandle<()> {
    let event_rx = event_bus.subscribe_domain();
    spawn_dm_bootstrap_handler_with_receiver(
        database,
        grpc_pool,
        message_service,
        event_rx,
        event_bus.clone(),
    )
}

/// Spawn the DM bootstrap event handler with a custom receiver.
///
/// Signal operations are handled asynchronously via the SignalActor.
pub fn spawn_dm_bootstrap_handler_with_receiver(
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    message_service: Arc<MessageService>,
    mut event_rx: broadcast::Receiver<DomainEvent>,
    event_bus: EventBus,
) -> JoinHandle<()> {
    // Bootstraps in progress, per peer DEVICE: a repeat from the same device
    // is dropped, while every device of a multi-device peer gets its own.
    let in_progress: Arc<Mutex<HashSet<(UserId, u32)>>> = Arc::new(Mutex::new(HashSet::new()));
    // One bootstrap task per peer at a time: two devices answering together
    // must not both find no mailbox of ours and each spend a token on one.
    // one entry per peer ever bootstrapped, never pruned.
    let peer_turns: Arc<Mutex<HashMap<UserId, Arc<Mutex<()>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Get user ID from grpc_pool for UUID comparison
    let auth_user_id = grpc_pool.auth_info().user_id.clone();

    tokio::spawn(async move {
        tracing::info!("DM bootstrap handler started");
        let pending_dms = PendingDmRepository::new(database.clone());
        let mut recovery = HashMap::<UserId, BootstrapWait>::new();
        let mut recovery_tick = tokio::time::interval(Duration::from_secs(30));
        recovery_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Per-bootstrap tasks are owned by this JoinSet. When the handler
        // exits (channel closed) or is aborted, the JoinSet drops and every
        // in-flight bootstrap task is aborted with it — no detached writes
        // continuing past the handler's lifetime.
        let mut bootstrap_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                _ = recovery_tick.tick() => {
                    match pending_dms.active_peers() {
                        Ok(peers) => {
                            recovery.retain(|peer, _| peers.contains(peer));
                            for peer in peers {
                                let decision = recovery.entry(peer).or_insert_with(BootstrapWait::new)
                                    .advance(Instant::now());
                                match decision {
                                    BootstrapDecision::Wait => {}
                                    BootstrapDecision::Reissue => {
                                        let service = message_service.clone();
                                        bootstrap_tasks.spawn(async move {
                                            if let Err(error) = service.reissue_dm_bootstrap(&peer).await {
                                                tracing::warn!(%peer, %error, "DM bootstrap reissue failed");
                                            }
                                        });
                                    }
                                    BootstrapDecision::Fail => {
                                        recovery.remove(&peer);
                                        match pending_dms.fail_peer(&peer, crate::services::message::core::BOOTSTRAP_TIMED_OUT) {
                                            Ok(contents) => {
                                                let mut flush = PendingFlush::default();
                                                for bytes in contents {
                                                    let bubble = crate::proto::content_v1::Content::decode(bytes.as_slice())
                                                        .ok().and_then(|content| queued_bubble_timestamp(&content));
                                                    flush.record(bubble, Err("Secure channel setup timed out. Tap Retry to try again".into()));
                                                }
                                                if !flush.failed.is_empty() {
                                                    flush.emit(peer, &event_bus);
                                                }
                                            }
                                            Err(error) => tracing::warn!(%peer, %error, "Could not park timed-out bootstrap"),
                                        }
                                    }
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "Could not inspect pending DM bootstraps"),
                    }
                },
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
                        // - MailboxAnnouncement::LEN = bootstrap message
                        //   (mailbox + conversation key + mailbox encryption key)
                        // - 0 bytes = bootstrap REQUEST (peer wants us to initiate)
                        // - 32 bytes = regular key exchange response
                        if DmBootstrap::is_bootstrap_message(&ephemeral_public_key) {
                        // Check if bootstrap is already in progress for this peer
                        {
                            let mut guard = in_progress.lock().await;
                            if !guard.insert((sender_id, sender_device_id)) {
                                tracing::debug!(
                                    peer = %sender_id,
                                    sender_device_id,
                                    "BOOTSTRAP[3/4] Skipping - already in progress for this device"
                                );
                                continue;
                            }
                        }

                        tracing::info!(
                            peer = %sender_id,
                            key_len = ephemeral_public_key.len(),
                            "BOOTSTRAP[3/4] Received mailbox-announcement KeyExchange = bootstrap MESSAGE from peer"
                        );

                        let database = database.clone();
                        let grpc_pool = grpc_pool.clone();
                        let ephemeral_key = ephemeral_public_key.clone();
                        let in_progress_clone = in_progress.clone();
                        let event_bus = event_bus.clone();
                        let message_service = message_service.clone();
                        let turn = peer_turn(&peer_turns, sender_id).await;

                        bootstrap_tasks.spawn(async move {
                            let _turn = turn.lock().await;
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
                                &message_service,
                                &event_bus,
                            )
                            .await;

                            // Release the global bootstrap lock for this peer
                            // (a concurrent send may hold it across its REQUEST).
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
                            in_progress_clone.lock().await.remove(&(sender_id, sender_device_id));

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
                            if !guard.insert((sender_id, sender_device_id)) {
                                tracing::debug!(
                                    peer = %sender_id,
                                    sender_device_id,
                                    "BOOTSTRAP[2/4] Skipping REQUEST - already in progress for this device"
                                );
                                continue;
                            }
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
                                in_progress.lock().await.remove(&(sender_id, sender_device_id));
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
                            let message_service = message_service.clone();
                            let in_progress_clone = in_progress.clone();
                            let turn = peer_turn(&peer_turns, sender_id).await;

                            bootstrap_tasks.spawn(async move {
                                let _turn = turn.lock().await;
                                if let Err(e) = handle_bootstrap_request_async(
                                    database,
                                    grpc_pool,
                                    message_service,
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
                                in_progress_clone.lock().await.remove(&(sender_id, sender_device_id));
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
                            in_progress.lock().await.remove(&(sender_id, sender_device_id));
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

const BOOTSTRAP_WAIT: Duration = Duration::from_secs(240);
const BOOTSTRAP_ATTEMPTS: u8 = 3;

struct BootstrapWait {
    since: Instant,
    attempts: u8,
}

enum BootstrapDecision { Wait, Reissue, Fail }

impl BootstrapWait {
    fn new() -> Self { Self { since: Instant::now(), attempts: 1 } }

    fn advance(&mut self, now: Instant) -> BootstrapDecision {
        if now.duration_since(self.since) < BOOTSTRAP_WAIT { return BootstrapDecision::Wait; }
        self.since = now;
        if self.attempts >= BOOTSTRAP_ATTEMPTS { BootstrapDecision::Fail }
        else { self.attempts += 1; BootstrapDecision::Reissue }
    }
}

#[cfg(test)]
#[test]
fn missing_bootstrap_reissues_twice_then_fails() {
    let start = Instant::now();
    let mut wait = BootstrapWait { since: start, attempts: 1 };
    assert!(matches!(wait.advance(start + BOOTSTRAP_WAIT - Duration::from_secs(1)), BootstrapDecision::Wait));
    assert!(matches!(wait.advance(start + BOOTSTRAP_WAIT), BootstrapDecision::Reissue));
    assert!(matches!(wait.advance(start + BOOTSTRAP_WAIT * 2), BootstrapDecision::Reissue));
    assert!(matches!(wait.advance(start + BOOTSTRAP_WAIT * 3), BootstrapDecision::Fail));
}

/// The lock that serialises `peer`'s bootstrap tasks.
async fn peer_turn(turns: &Mutex<HashMap<UserId, Arc<Mutex<()>>>>, peer: UserId) -> Arc<Mutex<()>> {
    turns.lock().await.entry(peer).or_default().clone()
}

/// Handle bootstrap REQUEST asynchronously.
///
/// When higher UUID peer requests bootstrap, we (lower UUID) send it our
/// bootstrap MESSAGE: our live mailbox and key again, or fresh ones.
///
/// Uses shared database and gRPC pool - no passphrase needed.
async fn handle_bootstrap_request_async(
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    message_service: Arc<MessageService>,
    peer_id: UserId,
    // The device that sent the REQUEST — the one waiting for our MESSAGE.
    peer_device_id: u32,
) -> Result<(), String> {
    use crate::mailbox::TokenManager;
    use crate::proto::chat_v1::SealedSenderMessage;

    // No early return on state we already hold: the REQUEST means the peer
    // has no mailbox of ours it can reach — whatever we hold. `initiate_dm`
    // re-announces a live mailbox and key, or makes fresh ones.
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
    let mut anon_mailbox_client = if message_service
        .shield_mode
        .load(std::sync::atomic::Ordering::Acquire)
    {
        grpc_pool
            .secret_mailbox_client()
            .map_err(|e| format!("Shield bootstrap needs Tor: {e}"))?
    } else {
        grpc_pool.anonymous_mailbox_client()
    };

    let bootstrap_result = dm_bootstrap
        .initiate_dm(peer_id, &mut anon_mailbox_client)
        .await
        .map_err(|e| format!("Failed to initiate bootstrap: {}", e))?;

    tracing::info!(
        peer = %peer_id,
        mailbox_id = %bootstrap_result.our_mailbox.id,
        bootstrap_msg_len = bootstrap_result.bootstrap_message.len(),
        "BOOTSTRAP[2/4] Created mailbox, now encrypting announcement MESSAGE for peer"
    );

    // Encrypt bootstrap message with Signal Protocol
    let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
    let key_store = Arc::new(KeyStore::new(database.clone()));
    let signal_service = SignalService::new(key_store, settings_repo);

    let mut auth_client = AuthClient::new(channel.clone(), auth_interceptor.clone());
    let peer_id_str = peer_id.to_string();

    let sealed_ciphertext = signal_service
        .encrypt_message(
            &mut auth_client,
            &peer_id_str,
            peer_device_id,
            &bootstrap_result.bootstrap_message,
        )
        .await
        .map_err(|e| format!("Failed to encrypt bootstrap: {}", e))?;

    // Spend a pre-fetched token; never request-then-spend (NDSS'21 §VI-F).
    let token = TokenManager::new(database.clone())
        .claim_prefetched_token()
        .map_err(|e| format!("No anonymous token for the bootstrap MESSAGE: {e}"))?;

    // Send to peer's device inbox via anonymous client (no AuthInterceptor)
    // to avoid dual-auth rejection — GRA-436
    let mut anon_client = if message_service
        .shield_mode
        .load(std::sync::atomic::Ordering::Acquire)
    {
        grpc_pool
            .secret_message_client()
            .map_err(|e| format!("Shield bootstrap needs Tor: {e}"))?
    } else {
        grpc_pool.anonymous_message_client()
    };

    let sealed_msg = SealedSenderMessage {
        sealed_body: sealed_ciphertext,
        server_key_id: signal_service.get_server_key_id(),
        timestamp: TimestampNanos::now_anonymized().as_u64(),
    };

    anon_client
        .send_with_anonymous_credential(
            &uuid::Uuid::new_v4().to_string(),
            &peer_id_str,
            vec![crate::proto::chat_v1::SealedDeviceMessage {
                device_id: peer_device_id,
                registration_id: 0,
                sealed_message: Some(sealed_msg),
            }],
            crate::proto::chat_v1::SealedDelivery::NamedDevices,
            0,
            &token.public_key,
            &token.signature,
        )
        .await
        .map_err(|e| format!("Failed to send bootstrap: {}", e))?;

    tracing::info!(
        peer = %peer_id,
        "BOOTSTRAP[2/4] SUCCESS - announcement MESSAGE sent to peer's device inbox"
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
    message_service: &MessageService,
    event_bus: &EventBus,
) -> Result<(), String> {
    tracing::info!(
        peer = %peer_id,
        key_len = ephemeral_public_key.len(),
        "BOOTSTRAP[3/4] Processing announcement MESSAGE - will send RESPONSE"
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
    let mut anon_mailbox_client = if message_service
        .shield_mode
        .load(std::sync::atomic::Ordering::Acquire)
    {
        grpc_pool
            .secret_mailbox_client()
            .map_err(|e| format!("Shield bootstrap needs Tor: {e}"))?
    } else {
        grpc_pool.anonymous_mailbox_client()
    };

    // A concurrent send may hold the lock across its REQUEST; taking the
    // MESSAGE over it beats `handle_incoming_bootstrap` dropping it.
    release_bootstrap_lock(&peer_id);

    // Process the incoming bootstrap
    tracing::info!(peer = %peer_id, "BOOTSTRAP[3/4] Calling handle_incoming_bootstrap");
    let response = match dm_bootstrap
        .handle_incoming_bootstrap(
            peer_id,
            peer_device_id,
            ephemeral_public_key,
            &mut anon_mailbox_client,
        )
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
        &mut anon_mailbox_client,
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
        message_service,
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
pub(crate) async fn send_pending_messages_after_bootstrap(
    peer_id: UserId,
    peer_device_id: u32,
    database: Arc<Database>,
    signal_service: &SignalService,
    auth_client: &mut AuthClient,
    channel: crate::network::grpc::transport::GrpcTransport,
    // Self-syncs what goes out, as the initiator's flush (`send_dm_content`) does.
    message_service: &MessageService,
) -> Option<PendingFlush> {
    use crate::mailbox::dm::DmConversationManager;
    use crate::storage::repositories::PendingDmRepository;
    use prost::Message as ProstMessage;

    let pending = match PendingDmRepository::new(database.clone()).drain(&peer_id) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!(peer = %peer_id, error = %e, "BOOTSTRAP[3/4] Failed to drain pending messages");
            return None;
        }
    };
    // Nothing queued is not nothing to send: a device answering after another
    // already took the queue is still owed its copies (drained below).
    let nothing_queued = pending.is_empty();
    tracing::info!(peer = %peer_id, count = pending.len(), "BOOTSTRAP[3/4] Sending pending messages (responder side)");

    // From here the queue is drained: every failure loses messages, so each
    // one is reported to the caller rather than only logged.
    let queued: Vec<_> = pending
        .into_iter()
        .map(|msg| {
            let decoded = crate::proto::content_v1::Content::decode(msg.content.as_slice()).ok();
            (decoded, msg.content)
        })
        .collect();
    let mut flush = PendingFlush::default();

    let conversations = DmConversationManager::new(database.clone());
    let conversation = conversations.load_conversation(&peer_id);
    // Just made by this device's bootstrap; the peer's other devices that have
    // no half yet are owed each copy until they answer.
    let (send_key, peer_mailbox, unready) = match conversation {
        Ok(Some(conv)) => {
            let ready = conv
                .ready_devices()
                .into_iter()
                .find(|(m, _)| m.device_id == peer_device_id)
                .map(|(m, key)| (key, m.clone()));
            let Some((key, mailbox)) = ready else {
                for (decoded, _) in queued {
                    flush.record(
                        decoded.as_ref().and_then(queued_bubble_timestamp),
                        Err("no mailbox keys after bootstrap".into()),
                    );
                }
                return Some(flush);
            };
            let unready: Vec<u32> = signal_service
                .session_devices(&peer_id.to_string())
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|d| !conv.peer_mailboxes.iter().any(|m| m.device_id == *d))
                .collect();
            (key, mailbox, unready)
        }
        Ok(None) => {
            for (decoded, _) in queued {
                flush.record(
                    decoded.as_ref().and_then(queued_bubble_timestamp),
                    Err("no conversation state after bootstrap".into()),
                );
            }
            return Some(flush);
        }
        Err(e) => {
            for (decoded, _) in queued {
                flush.record(
                    decoded.as_ref().and_then(queued_bubble_timestamp),
                    Err(e.to_string()),
                );
            }
            return Some(flush);
        }
    };

    let mut anon_mailbox_client = if message_service
        .shield_mode
        .load(std::sync::atomic::Ordering::Acquire)
    {
        match message_service.grpc_pool.secret_mailbox_client() {
            Ok(client) => client,
            Err(error) => {
                for (decoded, _) in queued {
                    flush.record(
                        decoded.as_ref().and_then(queued_bubble_timestamp),
                        Err(format!("Shield mailbox needs Tor: {error}")),
                    );
                }
                return Some(flush);
            }
        }
    } else {
        AnonymousMailboxClient::new(channel)
    };
    for (decoded, content) in queued {
        if decoded
            .as_ref()
            .is_some_and(|content| !message_service.queued_read_receipt_allowed(content))
        {
            continue;
        }
        // Receipt targets before transport, exactly as `send_dm_content`
        // records them: the peer device's Receipt names this DataMessage
        // timestamp and is applied only against a snapshot that has it.
        let snapshot = match &decoded {
            Some(decoded) => message_service
                .snapshot_dm_content_targets(peer_id, decoded, &[peer_mailbox.device_id])
                .map_err(|e| format!("receipt targets: {e}")),
            None => Ok(()),
        };
        let result = match snapshot {
            Ok(()) => {
                deposit(
                    signal_service,
                    auth_client,
                    &mut anon_mailbox_client,
                    &peer_id,
                    &peer_mailbox,
                    &send_key,
                    &content,
                )
                .await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = &result {
            tracing::error!(peer = %peer_id, error = %e, "Failed to send pending message");
        } else {
            for &device in &unready {
                if let Err(e) = conversations.queue_for_device(&peer_id, device, &content) {
                    tracing::warn!(peer = %peer_id, device, error = %e, "Owed copies: queue failed");
                }
            }
            // Self-synced when delivered, never when queued (F61 note b).
            if let Some(decoded) = &decoded {
                message_service
                    .sync_sent_to_own_devices(auth_client, &peer_id, decoded)
                    .await;
            }
        }
        flush.record(decoded.as_ref().and_then(queued_bubble_timestamp), result);
    }

    // This device answered after another got these copies: it catches up.
    // Already "sent" (another device has each), so a failure is only logged.
    match conversations.drain_device_queue(&peer_id, peer_device_id) {
        Ok(owed) => {
            for content in owed {
                if let Err(e) = deposit(
                    signal_service,
                    auth_client,
                    &mut anon_mailbox_client,
                    &peer_id,
                    &peer_mailbox,
                    &send_key,
                    &content,
                )
                .await
                {
                    tracing::warn!(peer = %peer_id, peer_device_id, error = %e, "Owed copy to a late device lost");
                }
            }
        }
        Err(e) => {
            tracing::warn!(peer = %peer_id, peer_device_id, error = %e, "Owed copies: drain failed")
        }
    }

    tracing::info!(
        peer = %peer_id,
        sent = flush.sent.len(),
        failed = flush.failed.len(),
        "Finished sending pending messages after bootstrap"
    );
    (!nothing_queued).then_some(flush)
}

/// Seal `content` for the device owning `peer_mailbox` and deposit it there.
async fn deposit(
    signal_service: &SignalService,
    auth_client: &mut AuthClient,
    anon_mailbox_client: &mut AnonymousMailboxClient,
    peer_id: &UserId,
    peer_mailbox: &crate::mailbox::dm::PeerMailbox,
    send_key: &[u8; 32],
    content: &[u8],
) -> Result<(), String> {
    for envelope in seal_for_peer_mailbox(
        signal_service,
        auth_client,
        peer_id,
        peer_mailbox.device_id,
        content,
        send_key,
    )
    .await?
    {
        anon_mailbox_client
            .send_to_ephemeral_mailbox(
                peer_mailbox.public_key.to_vec(),
                envelope,
                EnvelopeType::Message as i32,
            )
            .await
            .map_err(|e| format!("mailbox send: {e}"))?;
    }
    Ok(())
}

/// Seal `plaintext` for `peer_device_id` of `peer_id`, wrapped for the peer's
/// mailbox under `send_key`.
///
/// Only that device: it owns the mailbox and secret this copy goes under
/// (each peer device has its own), and its bootstrap left a session. Sealing to every session device also sealed to retired
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
        .encrypt_message_session_devices(
            auth_client,
            &peer_id.to_string(),
            vec![peer_device_id],
            plaintext,
        )
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
/// Layer 2 (Transport): HPKE envelope sealed to the peer mailbox's X25519
/// encryption key (not its Ed25519 address)
///
/// IMPORTANT: Uses the shared SignalService instance to ensure session counters
/// stay in sync with pending message sending.
async fn send_bootstrap_response_with_envelope(
    response: BootstrapResponse,
    peer_device_id: u32,
    our_device_id: u32,
    signal_service: &SignalService,
    auth_client: &mut AuthClient,
    anon_client: &mut AnonymousMailboxClient,
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

    // Layer 2: Wrap in an HPKE envelope sealed to the peer mailbox's
    // encryption key (announced beside its address in the bootstrap MESSAGE).
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
    let envelope = encrypt_envelope(&frame, &response.peer_mailbox_encryption_key)
        .map_err(|e| format!("Failed to encrypt envelope: {}", e))?;

    tracing::debug!(
        peer = %peer_id,
        envelope_len = envelope.len(),
        "Encrypted bootstrap response envelope"
    );

    // Send to peer's ephemeral mailbox (anonymous send, no auth required)
    // Uses anonymous client to avoid leaking device identity on anonymous endpoint
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
