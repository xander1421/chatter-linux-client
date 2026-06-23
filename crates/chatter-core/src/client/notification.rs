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
    ///
    /// This is the boot-time half: it spawns the domain-event → notification
    /// consumer, which needs neither an authenticated user nor a call service,
    /// so it's safe to start during `wire_all` before login. The auth-gated
    /// streams (gRPC message stream + call → notification bridge) start later
    /// via [`start_authed_notification_streams`] once login completes —
    /// otherwise a fresh user (no session at boot) would never get the call
    /// ring notification, because the bridge would have been skipped here.
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
    }

    /// Spawn the auth-gated notification streams: the gRPC message-notification
    /// stream and the call → notification bridge (`IncomingPresented` →
    /// `DomainEvent::IncomingCall` → ringing OS notification). Call after login
    /// (e.g. from initial-data load). Idempotent — runs its spawns once.
    pub fn start_authed_notification_streams(self: &Arc<Self>) {
        let user_id = match self.auth_state().get().current_user_id {
            Some(id) => id,
            None => {
                tracing::debug!("skipping authed notification streams — no authenticated user yet");
                return;
            }
        };
        // Only spawn once; guarded so a re-login / repeated initial-data load
        // doesn't stack duplicate streams.
        if self
            .authed_notif_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }

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
            tracing::info!("call → notification bridge started");
        } else {
            tracing::warn!("call service not ready — call ring notifications disabled this session");
        }
    }
}
