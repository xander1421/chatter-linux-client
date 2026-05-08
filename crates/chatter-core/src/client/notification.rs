//! Notification listener wiring on `ChatterClient`.
//!
//! Lives outside `jarvis.rs` because the listener spans device/call/session
//! events independent of the jarvis feature.

use std::sync::Arc;

use super::ChatterClient;

impl ChatterClient {
    /// Spawn a background task that routes domain events to privacy-aware
    /// desktop notifications. Respects the active conversation (no notifications
    /// for the channel the user is currently viewing) and skips own messages.
    ///
    /// The task runs for the lifetime of the broadcast channel.
    pub fn start_notification_listener(self: &Arc<Self>) {
        let handle = crate::services::notification_service::spawn_notification_listener(
            self.event_bus().clone(),
            self.auth_state().clone(),
            self.chat_state().clone(),
            self.users().clone(),
            self.notifications().clone(),
            self.signal().clone(),
            self.ui.channel_repo.clone(),
        );
        self.tasks.lock().push(handle);

        let user_id = match self.auth_state().get().current_user_id {
            Some(id) => id,
            None => {
                tracing::debug!("skipping gRPC notification stream — no authenticated user");
                return;
            }
        };
        let stream_handle = crate::services::notification_stream::spawn_grpc_notification_consumer(
            self.ui.grpc_pool.clone(),
            self.event_bus().clone(),
            user_id,
        );
        self.tasks.lock().push(stream_handle);

        #[cfg(feature = "media")]
        if let Some(call_worker) = self.ui.call_service.get() {
            let bridge_handle = crate::services::call_notification_bridge::spawn_call_notification_bridge(
                call_worker,
                self.event_bus().clone(),
            );
            self.tasks.lock().push(bridge_handle);
        }
    }
}
