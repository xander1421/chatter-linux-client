//! Friend requests polling.
//!
//! Handles periodic polling of friend requests to keep the UI up-to-date
//! when new friend requests arrive.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;

use crate::inbox::events::{FriendRequestInfo, InboxEvent};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::friend_requests_filter;

/// Minimum interval between friend request polls (30 seconds).
const MIN_POLL_INTERVAL_SECS: u64 = 30;

/// State for friend request polling (tracks last poll time).
pub struct FriendRequestPoller {
    last_poll: Option<Instant>,
}

impl FriendRequestPoller {
    /// Create a new friend request poller.
    pub fn new() -> Self {
        Self { last_poll: None }
    }

    /// Check if enough time has passed since the last poll.
    pub fn should_poll(&self) -> bool {
        match self.last_poll {
            Some(last) => last.elapsed().as_secs() >= MIN_POLL_INTERVAL_SECS,
            None => true, // First poll
        }
    }

    /// Mark that a poll just occurred.
    pub fn mark_polled(&mut self) {
        self.last_poll = Some(Instant::now());
    }
}

impl Default for FriendRequestPoller {
    fn default() -> Self {
        Self::new()
    }
}

/// Poll for friend requests and emit an event if there are any.
///
/// This polls both received and sent friend requests from the server
/// and emits a `FriendRequestsUpdated` event so the UI can update.
pub async fn poll_friend_requests(
    grpc_pool: &Arc<GrpcClientPool>,
    event_tx: &broadcast::Sender<InboxEvent>,
    poller: &mut FriendRequestPoller,
) {
    // Check if we should poll (rate limiting)
    if !poller.should_poll() {
        return;
    }

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
            })
            .collect(),
        Err(e) => {
            tracing::debug!("Failed to poll received friend requests: {}", e);
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
            })
            .collect(),
        Err(e) => {
            tracing::debug!("Failed to poll sent friend requests: {}", e);
            return;
        }
    };

    // Mark that we polled successfully
    poller.mark_polled();

    // Emit event
    let _ = event_tx.send(InboxEvent::FriendRequestsUpdated { received, sent });
}
