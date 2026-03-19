//! User events stream handler.
//!
//! Subscribes to the server's user events stream for real-time notifications
//! of friend request changes, profile updates, and presence changes.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::inbox::events::{FriendRequestInfo, InboxEvent};
use crate::proto::chat_v1::FriendStatus;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::{friend_requests_filter, user_event};

/// Spawn the user events stream handler.
///
/// This function spawns a task that subscribes to the user events stream
/// and processes events as they arrive. When friend status changes, it
/// triggers a refresh of the friend request lists.
///
/// # Arguments
///
/// * `user_id` - The current user's ID
/// * `grpc_pool` - gRPC client pool for creating user client
/// * `event_tx` - Broadcast sender for inbox events
/// * `shutdown_rx` - Watch receiver for shutdown signal
pub async fn spawn_user_events_stream(
    user_id: String,
    grpc_pool: Arc<GrpcClientPool>,
    event_tx: broadcast::Sender<InboxEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            // Check for shutdown
            if *shutdown_rx.borrow() {
                tracing::debug!("User events stream shutting down");
                break;
            }

            // Subscribe to user events stream
            let mut user_client = grpc_pool.user_client();

            match user_client.stream_user_events(&user_id).await {
                Ok(mut stream) => {
                    tracing::info!("Subscribed to user events stream");

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    tracing::debug!("User events stream received shutdown signal");
                                    return;
                                }
                            }
                            event = stream.next() => {
                                match event {
                                    Some(Ok(user_event)) => {
                                        handle_user_event(
                                            user_event,
                                            &grpc_pool,
                                            &event_tx,
                                        ).await;
                                    }
                                    Some(Err(e)) => {
                                        tracing::warn!("Error receiving user event: {}", e);
                                        break; // Reconnect
                                    }
                                    None => {
                                        tracing::debug!("User events stream ended");
                                        break; // Reconnect
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to subscribe to user events stream: {}", e);
                }
            }

            // Wait before reconnecting (unless shutting down)
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        return;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    });
}

/// Handle a user event from the stream.
async fn handle_user_event(
    event: crate::proto::chat_v1::UserEvent,
    grpc_pool: &Arc<GrpcClientPool>,
    event_tx: &broadcast::Sender<InboxEvent>,
) {
    match event.event {
        Some(user_event::Event::FriendStatusChanged(status)) => {
            tracing::debug!(
                user_id = %status.user_id,
                status = ?status.status,
                channel_id = %status.channel_id,
                "Friend status changed event received"
            );

            // If friend was accepted AND we got a channel_id, emit FriendAccepted event
            if status.status == FriendStatus::Accepted as i32 && !status.channel_id.is_empty() {
                tracing::info!(
                    friend_user_id = %status.user_id,
                    channel_id = %status.channel_id,
                    "Friend accepted with DM channel - emitting FriendAccepted event"
                );
                let _ = event_tx.send(InboxEvent::FriendAccepted {
                    friend_user_id: status.user_id.clone(),
                    channel_id: status.channel_id.clone(),
                });
            }

            // Refresh friend request lists
            refresh_friend_requests(grpc_pool, event_tx).await;
        }
        Some(user_event::Event::ProfileUpdated(profile)) => {
            tracing::debug!(
                user_id = %profile.user_id,
                "Profile updated event received"
            );
            // TODO: Handle profile updates if needed
        }
        Some(user_event::Event::PresenceChanged(presence)) => {
            tracing::trace!(
                user_id = %presence.user_id,
                presence = ?presence.presence,
                "Presence changed event received"
            );
            // TODO: Handle presence updates
        }
        None => {
            tracing::trace!("Received empty user event");
        }
    }
}

/// Refresh friend request lists and emit update event.
async fn refresh_friend_requests(
    grpc_pool: &Arc<GrpcClientPool>,
    event_tx: &broadcast::Sender<InboxEvent>,
) {
    let mut user_client = grpc_pool.user_client();

    // Fetch received requests
    let received = match user_client
        .list_friend_requests(friend_requests_filter::Filter::Received, 50, 0)
        .await
    {
        Ok(response) => response
            .requests
            .into_iter()
            .map(|r| FriendRequestInfo {
                user_id: r.user_id,
                display_name: r.display_name,
                username: r.username,
                avatar_url: r.avatar_url,
            })
            .collect(),
        Err(e) => {
            tracing::debug!("Failed to refresh received friend requests: {}", e);
            return;
        }
    };

    // Fetch sent requests
    let sent = match user_client
        .list_friend_requests(friend_requests_filter::Filter::Sent, 50, 0)
        .await
    {
        Ok(response) => response
            .requests
            .into_iter()
            .map(|r| FriendRequestInfo {
                user_id: r.user_id,
                display_name: r.display_name,
                username: r.username,
                avatar_url: r.avatar_url,
            })
            .collect(),
        Err(e) => {
            tracing::debug!("Failed to refresh sent friend requests: {}", e);
            return;
        }
    };

    // Emit event
    let _ = event_tx.send(InboxEvent::FriendRequestsUpdated { received, sent });
    tracing::debug!("Friend requests refreshed via stream event");
}
