//! Server-managed direct message stream listener.
//!
//! Subscribes to `MessageService::StreamMessages` for a single
//! server-managed DM channel (`ChannelType::DirectMessage`) and drives the
//! incoming wire `Message` proto into the unified message repository as
//! `ConversationType::ServerDm`. Each accepted insert raises a
//! `DomainEvent::MessageReceived` so the rest of the client (notification
//! sink, UI state stores, etc.) can react in real time.
//!
//! # Why a stream and not the mailbox poller
//!
//! Before this listener landed, server-managed DMs were caught — by
//! accident — in `inbox/polling/dm.rs` and stored as `MailboxDm`, the same
//! bucket used for mailbox/sealed-sender private DMs. The server-managed
//! path is supposed to be stream-driven: the server already knows the
//! sender, the channel is plain server storage, and there is no mailbox
//! envelope. Polling for them was redundant and put rows in the wrong
//! conversation_type bucket. The mailbox poller now skips DM-typed
//! channels (see `polling/dm.rs`) and this listener covers them instead.
//!
//! # Lifecycle
//!
//! Spawned once per DM channel at login from the inbox provider. The task
//! loops forever: on stream end or transport error it logs and reconnects
//! after a fixed backoff, mirroring `services::channel_event_service`.
//! Self-sent messages (echoed back from the server when this device sends
//! a DM) are filtered out by `current_user_id` so we don't double-store
//! the row that the send path already inserted locally.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::task::JoinHandle;

use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::ConversationType;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::message::proto_to_domain_message_with_sender;
use crate::storage::repositories::MessageRepository;

/// Reconnect delay after stream end / transport error.
///
/// Matches `services::channel_event_service` — long enough not to hot-loop
/// against a flapping server, short enough that the user perceives the
/// reconnect as transparent.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Spawn a `MessageService::StreamMessages` listener for a single
/// server-managed DM channel.
///
/// Returns a `JoinHandle` so the caller can keep the task alive (drop =
/// abort). The task runs until cancelled; it does not observe the inbox
/// provider's shutdown channel because nothing in the surrounding boot
/// code does — see `streaming::dm_bootstrap_handler` for the same shape.
///
/// # Arguments
///
/// * `channel_id` - DM channel to subscribe to. Must already be a row in
///   `channels` with `channel_type = DirectMessage`.
/// * `grpc_pool` - Shared gRPC connection pool. The listener creates a
///   fresh `MessageClient` per reconnect attempt so a stale HTTP/2
///   connection on one reconnect doesn't poison the next.
/// * `event_bus` - For emitting `DomainEvent::MessageReceived` on each new
///   row.
/// * `message_repo` - Unified message repository (insert_or_ignore on the
///   sender_id+sent_at primary key handles dedup against the optimistic
///   send-side insert).
/// * `current_user_id` - Used to skip our own messages echoed back by the
///   server stream.
pub fn spawn_server_dm_listener(
    channel_id: ChannelId,
    grpc_pool: Arc<GrpcClientPool>,
    event_bus: EventBus,
    message_repo: Arc<MessageRepository>,
    current_user_id: UserId,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            channel_id = %channel_id,
            "ServerDm stream listener starting"
        );

        loop {
            match run_stream(&channel_id, &grpc_pool, &event_bus, &message_repo, &current_user_id).await {
                Ok(()) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        "ServerDm stream ended cleanly, reconnecting after backoff"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "ServerDm stream error, reconnecting after backoff"
                    );
                }
            }

            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    })
}

/// Open a stream and drain it until it ends or errors.
///
/// Returns `Ok(())` on clean stream end (server closed), `Err(msg)` on
/// transport / decode error. The outer loop in `spawn_server_dm_listener`
/// reconnects either way.
async fn run_stream(
    channel_id: &ChannelId,
    grpc_pool: &Arc<GrpcClientPool>,
    event_bus: &EventBus,
    message_repo: &Arc<MessageRepository>,
    current_user_id: &UserId,
) -> Result<(), String> {
    let channel_id_str = channel_id.to_string();

    // Fresh client per attempt — see doc on `spawn_server_dm_listener`.
    let mut client = grpc_pool.message_client();

    let stream = client
        .stream_messages(&channel_id_str, None)
        .await
        .map_err(|e| format!("subscribe failed: {}", e))?;

    tracing::info!(
        channel_id = %channel_id,
        "ServerDm stream subscribed"
    );

    tokio::pin!(stream);

    while let Some(item) = stream.next().await {
        match item {
            Ok(proto_msg) => {
                handle_proto_message(
                    proto_msg,
                    channel_id,
                    event_bus,
                    message_repo,
                    current_user_id,
                );
            }
            Err(e) => {
                return Err(format!("stream item error: {}", e));
            }
        }
    }

    Ok(())
}

/// Handle a single proto Message from the stream.
///
/// Steps: parse sender, drop self-echoes, convert to domain Message tagged
/// `ConversationType::ServerDm`, insert_or_ignore, emit
/// `DomainEvent::MessageReceived` on fresh inserts.
fn handle_proto_message(
    proto: crate::proto::chat_v1::Message,
    channel_id: &ChannelId,
    event_bus: &EventBus,
    message_repo: &Arc<MessageRepository>,
    current_user_id: &UserId,
) {
    // Parse sender. Server stream always carries author_id for
    // server-managed DMs (no sealed-sender on this path).
    let sender_id: UserId = match proto.author_id.parse() {
        Ok(id) => id,
        Err(_) => {
            tracing::warn!(
                channel_id = %channel_id,
                author_id = %proto.author_id,
                message_id = %proto.id,
                "ServerDm stream: invalid author_id, skipping"
            );
            return;
        }
    };

    // Skip our own messages echoed back. The send path already inserted
    // the row locally (so the UI shows the message immediately on send).
    if &sender_id == current_user_id {
        tracing::debug!(
            channel_id = %channel_id,
            message_id = %proto.id,
            "ServerDm stream: skipping self-echo"
        );
        return;
    }

    // Convert to domain. ConversationType::ServerDm is the load-bearing
    // bit — `domain/channel.rs::Channel::conversation_type()` already
    // maps DirectMessage → ServerDm; we mirror it here so the row lands
    // in the right bucket regardless of whether a channel-row lookup
    // would succeed at this exact moment.
    let message = match proto_to_domain_message_with_sender(
        &proto,
        Some(sender_id),
        ConversationType::ServerDm,
    ) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                channel_id = %channel_id,
                message_id = %proto.id,
                error = %e,
                "ServerDm stream: proto→domain conversion failed, skipping"
            );
            return;
        }
    };

    match message_repo.insert_or_ignore(&message) {
        Ok(true) => {
            tracing::debug!(
                channel_id = %channel_id,
                message_id = %message.id,
                sender_id = %sender_id,
                "ServerDm stream: stored new message"
            );

            // Emit so notification sink + UI state stores update.
            // Duplicates (Ok(false)) intentionally do not fire — same
            // contract as polling/dm.rs's Text branch.
            let _ = event_bus.domain_sender().send(DomainEvent::MessageReceived {
                message,
            });
        }
        Ok(false) => {
            tracing::debug!(
                channel_id = %channel_id,
                message_id = %message.id,
                "ServerDm stream: duplicate, ignored"
            );
        }
        Err(e) => {
            tracing::warn!(
                channel_id = %channel_id,
                message_id = %message.id,
                error = %e,
                "ServerDm stream: insert failed"
            );
        }
    }
}
