//! User events stream handler.
//!
//! Subscribes to the server's user events stream for real-time notifications
//! of friend request changes, profile updates, and presence changes.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::domain::ids::UserId;
use crate::domain::user::{FriendshipStatus, Profile};
use crate::inbox::events::{FriendRequestInfo, InboxEvent};
use crate::inbox::handlers::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::streaming::heartbeat::{self, Next};
use crate::proto::UserPresence;
use crate::proto::chat_v1::FriendStatus;
use crate::proto::chat_v1::{friend_requests_filter, user_event};
use crate::services::user::PROFILE_REFRESH_INTERVAL;
use crate::services::user::conversions::proto_to_domain_profile;
use crate::storage::repositories::{SettingsRepository, UserRepository};

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
    settings_repo: Arc<SettingsRepository>,
    domain_tx: Option<broadcast::Sender<DomainEvent>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    // Parsed once for the friend-key lookup that decrypts inbound profile names.
    let current_user_id = user_id.parse::<UserId>().ok();

    // Return the handle so the caller can ABORT this task on transport switch.
    // The old stream holds a channel clone pinned to the pre-switch transport
    // (direct); relying on the shared shutdown watch alone races with its reset,
    // leaving a direct, IP-revealing socket alive in shield mode. Abort is
    // unconditional and immediate.
    tokio::spawn(async move {
        let mut last_profile_refresh: Option<Instant> = None;
        loop {
            // Check for shutdown
            if *shutdown_rx.borrow() {
                tracing::debug!("User events stream shutting down");
                break;
            }

            // Subscribe to user events stream
            let mut user_client = grpc_pool.user_client();

            let mut connected_at = None;
            match user_client.stream_user_events(&user_id).await {
                Ok(mut stream) => {
                    connected_at = Some(Instant::now());
                    tracing::info!("Subscribed to user events stream");

                    // Friend requests and profile edits arrive only as pushes
                    // on this stream; refetch what a down stream missed. After
                    // subscribing, so nothing falls between the two. Profiles
                    // cost one RPC per friend, so they share
                    // `refresh_profile_if_stale`'s interval.
                    refresh_friend_requests(&grpc_pool, &event_tx).await;
                    if let Some(me) = current_user_id {
                        let due = last_profile_refresh
                            .map_or(true, |t: Instant| t.elapsed() >= PROFILE_REFRESH_INTERVAL);
                        if due {
                            last_profile_refresh = Some(Instant::now());
                            let (grpc_pool, user_repo, domain_tx) =
                                (grpc_pool.clone(), user_repo.clone(), domain_tx.clone());
                            tokio::spawn(async move {
                                refresh_friend_profiles(
                                    &grpc_pool,
                                    &user_repo,
                                    &me,
                                    domain_tx.as_ref(),
                                )
                                .await;
                            });
                        }
                    }

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    tracing::debug!("User events stream received shutdown signal");
                                    return;
                                }
                            }
                            event = heartbeat::next_or_silent(&mut stream, heartbeat::STREAM_SILENCE_LIMIT) => {
                                match event {
                                    Next::Item(Ok(user_event)) => {
                                        handle_user_event(
                                            user_event,
                                            &grpc_pool,
                                            &event_tx,
                                            &user_repo,
                                            &settings_repo,
                                            current_user_id.as_ref(),
                                            domain_tx.as_ref(),
                                        ).await;
                                    }
                                    Next::Item(Err(e)) => {
                                        tracing::warn!("Error receiving user event: {}", e);
                                        break; // Reconnect
                                    }
                                    Next::Ended => {
                                        tracing::debug!("User events stream ended");
                                        break; // Reconnect
                                    }
                                    Next::Silent => {
                                        tracing::warn!("User events stream missed its heartbeats; reconnecting");
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

            // Wait before reconnecting (unless shutting down). A stream that had
            // been healthy for a while reconnects at once: its loss is routine
            // (server close, network change), and friend requests and presence
            // pushed in the gap are only refetched once we are back.
            let wait = if connected_at
                .is_some_and(|t: Instant| t.elapsed() >= heartbeat::HEALTHY_STREAM_AFTER)
            {
                std::time::Duration::from_millis(250)
            } else {
                std::time::Duration::from_secs(5)
            };
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        return;
                    }
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }
    })
}

/// Resolve a profile display name that arrives on the user-events stream.
///
/// Server-stored display names are encrypted with the owner's profile key, so
/// the raw `UserProfile.display_name` here is base64 ciphertext and must never
/// be persisted or shown. We decrypt it when it belongs to a friend whose
/// profile key we hold; otherwise we keep the name already in local storage
/// (this covers our own profile, whose plaintext is set at login) and finally
/// fall back to a stable `User <id>` placeholder.
fn resolve_stream_display_name(
    user_repo: &UserRepository,
    current_user_id: Option<&UserId>,
    target: &UserId,
    raw: &str,
) -> String {
    // Decrypt with the friend's profile key; else keep the name we already
    // have; else a placeholder. Never persist the undecryptable server value.
    current_user_id
        .and_then(|cur| {
            crate::services::user::decrypt_friend_display_name(user_repo, cur, target, raw)
        })
        .or_else(|| {
            user_repo
                .get_profile(target)
                .ok()
                .flatten()
                .map(|p| p.display_name)
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| crate::services::user::friend_display_name_placeholder(target))
}

/// Handle a user event from the stream.
async fn handle_user_event(
    event: crate::proto::chat_v1::UserEvent,
    grpc_pool: &Arc<GrpcClientPool>,
    event_tx: &broadcast::Sender<InboxEvent>,
    user_repo: &Arc<UserRepository>,
    settings_repo: &SettingsRepository,
    current_user_id: Option<&UserId>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    match event.event {
        // The server's keepalive (also its first item once subscribed): not data.
        Some(user_event::Event::Heartbeat(_)) => {
            tracing::trace!("User events stream heartbeat");
        }
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
            if current_user_id.is_some_and(|me| me.to_string() == proto_profile.user_id) {
                apply_own_profile(&proto_profile, user_repo, settings_repo, domain_tx);
            } else {
                apply_profile(&proto_profile, user_repo, current_user_id, domain_tx, true);
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

            let proto_presence =
                UserPresence::try_from(presence.presence).unwrap_or(UserPresence::Offline);
            let domain_presence =
                crate::services::presence_service::proto_presence_to_domain(proto_presence);

            // Persist before emitting. The event only patches the rows that are
            // on screen right now; every list that rebuilds from storage (the DM
            // sidebar, the Contacts tab, the header on reopen) reads
            // `profiles.presence`, so without this a contact's dot went green on
            // the live event and back to grey at the next rebuild.
            // `PresenceUpdate.timestamp` is unix NANOseconds; `last_seen_at`
            // stores seconds. A zero timestamp means the server sent none, in
            // which case keep whatever we already had rather than stamping the
            // epoch.
            let last_seen_at = match presence.timestamp {
                0 => None,
                ns => Some((ns / 1_000_000_000) as i64),
            };
            if let Err(e) = user_repo.set_presence(&user_id, domain_presence, last_seen_at) {
                tracing::warn!(error = %e, %user_id, "Failed to persist peer presence");
            }

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

/// Decrypt a server profile, persist it, and emit `UserProfileUpdated` — for a
/// live push always, for a refetch only when something visible changed.
fn apply_profile(
    proto: &crate::proto::chat_v1::UserProfile,
    user_repo: &UserRepository,
    current_user_id: Option<&UserId>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    announce_unchanged: bool,
) {
    let mut profile = match proto_to_domain_profile(proto) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to convert proto profile");
            return;
        }
    };

    // `proto_to_domain_profile` copies the raw server value, which is
    // ciphertext (display names are stored encrypted). Resolve it to a
    // safe name so we never persist/show ciphertext.
    profile.display_name = resolve_stream_display_name(
        user_repo,
        current_user_id,
        &profile.user_id,
        &proto.display_name,
    );

    // Status is E2E-encrypted too — decrypt with the friend's key, else
    // drop it (never persist/emit ciphertext).
    profile.status_message = current_user_id
        .and_then(|cur| {
            crate::services::user::decrypt_friend_status(
                user_repo,
                cur,
                &profile.user_id,
                &proto.status_message,
            )
        })
        .map(crate::domain::user::StatusMessage::new);

    // Avatar too, or a peer's new photo pushed here never shows (the
    // upsert keeps the old one when this is `None`).
    profile.avatar_bytes = current_user_id
        .filter(|_| !proto.encrypted_avatar.is_empty())
        .and_then(|cur| {
            user_repo
                .get_friend_profile_key(cur, &profile.user_id)
                .ok()
                .flatten()
        })
        .and_then(|key| {
            crate::crypto::profile_crypto::decrypt_avatar(&key, &proto.encrypted_avatar).ok()
        });

    let before = user_repo.get_profile(&profile.user_id).ok().flatten();

    // A profile push may arrive before the friends sync has created this
    // user. `profiles.user_id` has a foreign key to `users.id`.
    if let Err(e) = user_repo.ensure_user_exists(&profile.user_id) {
        tracing::warn!(error = %e, user_id = %profile.user_id, "Failed to create profile user");
        return;
    }
    if let Err(e) = user_repo.upsert_profile(&profile) {
        tracing::warn!(error = %e, user_id = %profile.user_id, "Failed to upsert profile");
        return;
    }

    if !announce_unchanged && !profile_changed(before.as_ref(), &profile) {
        return;
    }

    if let Some(tx) = domain_tx {
        let _ = tx.send(DomainEvent::UserProfileUpdated {
            user_id: profile.user_id,
            display_name: profile.display_name.clone(),
            status_message: profile.status_message.as_ref().map(|s| s.text.clone()),
            avatar_bytes: profile.avatar_bytes.clone(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        });
    }
}

/// Our OWN profile, pushed because a device of this account (this one or a
/// linked one) edited it. It is encrypted under our own profile key, not a
/// friend key, so it takes the fetch path's own-profile restore, which also
/// rewrites the local copies (`keys::DISPLAY_NAME`, `keys::AVATAR`) the UI
/// reads. The editing device receives the echo too; it decrypts to what it
/// just saved.
fn apply_own_profile(
    proto: &crate::proto::chat_v1::UserProfile,
    user_repo: &UserRepository,
    settings_repo: &SettingsRepository,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    let mut profile = match proto_to_domain_profile(proto) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to convert own proto profile");
            return;
        }
    };
    if let Err(e) =
        crate::services::user::restore_own_profile(settings_repo, user_repo, &mut profile, proto)
    {
        tracing::warn!(error = %e, "Failed to restore pushed own profile");
        return;
    }
    if let Err(e) = user_repo
        .ensure_user_exists(&profile.user_id)
        .and_then(|()| user_repo.upsert_profile(&profile))
    {
        tracing::warn!(error = %e, "Failed to store pushed own profile");
        return;
    }
    if let Some(tx) = domain_tx {
        let _ = tx.send(DomainEvent::UserProfileUpdated {
            user_id: profile.user_id,
            display_name: profile.display_name.clone(),
            status_message: profile.status_message.as_ref().map(|s| s.text.clone()),
            avatar_bytes: profile.avatar_bytes.clone(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        });
    }
}

/// Did name, status or avatar change? Presence has its own event. An avatar of
/// `None` means "not decrypted", and the upsert keeps the stored one.
fn profile_changed(before: Option<&Profile>, after: &Profile) -> bool {
    let Some(before) = before else { return true };
    before.display_name != after.display_name
        || before.status_message.as_ref().map(|s| &s.text)
            != after.status_message.as_ref().map(|s| &s.text)
        || (after.avatar_bytes.is_some() && before.avatar_bytes != after.avatar_bytes)
}

/// Refetch every accepted friend's profile and announce what changed.
///
/// Profile edits reach us only as a live push on this stream (the server's
/// `publishProfileUpdate`), so an edit made while the stream was down was
/// missed until something else happened to refetch that profile.
async fn refresh_friend_profiles(
    grpc_pool: &GrpcClientPool,
    user_repo: &UserRepository,
    current_user_id: &UserId,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    let friends = match user_repo.list_friends(current_user_id) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to list friends for profile refresh");
            return;
        }
    };
    let mut client = grpc_pool.user_client();
    for f in friends
        .iter()
        .filter(|f| f.status == FriendshipStatus::Accepted)
    {
        let peer = if f.requester_id == *current_user_id {
            f.addressee_id
        } else {
            f.requester_id
        };
        match client.get_user_profile(&peer.to_string()).await {
            Ok(proto) => apply_profile(&proto, user_repo, Some(current_user_id), domain_tx, false),
            Err(e) => tracing::debug!(%peer, error = %e, "Friend profile refresh failed"),
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
                encrypted_intro: r.encrypted_message,
                created_at_ns: r.added_at,
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
                encrypted_intro: r.encrypted_message,
                created_at_ns: r.added_at,
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

#[cfg(test)]
mod heartbeat_tests {
    use super::*;
    use crate::proto::chat_v1::{StreamHeartbeat, UserEvent};

    /// The server's keepalive must not touch state: no inbox event (which
    /// would refetch friend requests every minute), no domain event.
    #[tokio::test]
    async fn a_heartbeat_changes_nothing() {
        let db = crate::services::channel::membership_log::test_support::test_db();
        let settings_repo =
            crate::storage::repositories::settings_repo::SettingsRepository::new(db.clone());
        let user_repo = Arc::new(UserRepository::new(db));
        // Never reachable; a heartbeat must not make any request.
        let pool = Arc::new(GrpcClientPool::new("https://127.0.0.1:1").unwrap());
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let (domain_tx, mut domain_rx) = broadcast::channel(4);

        handle_user_event(
            UserEvent {
                timestamp: 1,
                event: Some(user_event::Event::Heartbeat(StreamHeartbeat {})),
            },
            &pool,
            &event_tx,
            &user_repo,
            &settings_repo,
            None,
            Some(&domain_tx),
        )
        .await;

        assert!(
            event_rx.try_recv().is_err(),
            "a heartbeat produced an inbox event"
        );
        assert!(
            domain_rx.try_recv().is_err(),
            "a heartbeat produced a domain event"
        );
    }
}
