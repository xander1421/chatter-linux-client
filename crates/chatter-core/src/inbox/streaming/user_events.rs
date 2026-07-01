//! User events stream handler.
//!
//! Subscribes to the server's user events stream for real-time notifications
//! of friend request changes, profile updates, and presence changes.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::inbox::events::{FriendRequestInfo, InboxEvent};
use crate::inbox::handlers::DomainEvent;
use crate::proto::chat_v1::FriendStatus;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::{friend_requests_filter, user_event};
use crate::proto::UserPresence;
use crate::services::user::conversions::proto_to_domain_profile;
use crate::storage::repositories::UserRepository;

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
    user_repo: Arc<UserRepository>,
    domain_tx: Option<broadcast::Sender<DomainEvent>>,
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
                                            &user_repo,
                                            domain_tx.as_ref(),
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
    user_repo: &Arc<UserRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    match event.event {
        Some(user_event::Event::FriendStatusChanged(status)) => {
            tracing::debug!(
                user_id = %status.user_id,
                status = ?status.status,
                "Friend status changed event received"
            );

            // If friend was accepted, emit FriendAccepted event. DMs are
            // channel-free (Signal-style) — there is no server DM channel
            // to wait for; the peer-keyed local conversation is created
            // client-side by the domain event handler.
            if status.status == FriendStatus::Accepted as i32 {
                tracing::info!(
                    friend_user_id = %status.user_id,
                    "Friend accepted - emitting FriendAccepted event"
                );
                let _ = event_tx.send(InboxEvent::FriendAccepted {
                    friend_user_id: status.user_id.clone(),
                });
            }

            // Refresh friend request lists
            refresh_friend_requests(grpc_pool, event_tx).await;
        }
        Some(user_event::Event::ProfileUpdated(proto_profile)) => {
            tracing::debug!(
                user_id = %proto_profile.user_id,
                "Profile updated event received"
            );

            let profile = match proto_to_domain_profile(&proto_profile) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to convert proto profile");
                    return;
                }
            };

            if let Err(e) = user_repo.upsert_profile(&profile) {
                tracing::warn!(error = %e, user_id = %profile.user_id, "Failed to upsert profile");
                return;
            }

            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::UserProfileUpdated {
                    user_id: profile.user_id,
                    display_name: profile.display_name.clone(),
                    status_message: profile.status_message.as_ref().map(|s| s.text.clone()),
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                });
            }
        }
        Some(user_event::Event::PresenceChanged(presence)) => {
            tracing::trace!(
                user_id = %presence.user_id,
                presence = ?presence.presence,
                "Presence changed event received"
            );

            let user_id = match presence.user_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::debug!(user_id = %presence.user_id, "Skipping presence event with invalid user_id");
                    return;
                }
            };

            let proto_presence = UserPresence::try_from(presence.presence)
                .unwrap_or(UserPresence::Offline);
            let domain_presence = crate::services::presence_service::proto_presence_to_domain(
                proto_presence,
            );

            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::UserPresenceChanged {
                    user_id,
                    presence: domain_presence,
                    timestamp: presence.timestamp,
                });
            }
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
