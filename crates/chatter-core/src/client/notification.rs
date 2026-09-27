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
            Arc::downgrade(self),
            self.users().clone(),
            self.notifications().clone(),
            self.signal().clone(),
            self.services.channel_repo.clone(),
        );
        self.tasks.lock().push(handle);
    }

    /// Spawn the auth-gated notification streams: the gRPC message-notification
    /// stream and the call → notification bridge (`IncomingPresented` →
    /// ringing OS notification, cancelled when the ring ends). Call after login
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

        // The gRPC notification stream is held in its own slot (not the `tasks`
        // bag) so an anon-mode transport switch can abort + re-spawn just this
        // loop. The call → notification bridge below carries no server
        // connection, so it stays a one-time spawn in `tasks`.
        self.spawn_notification_stream(user_id);

        #[cfg(feature = "media")]
        if let Some(call_worker) = self.services.call_service.get() {
            let bridge_handle = crate::services::call_notification_bridge::spawn_call_notification_bridge(
                call_worker.subscribe_call_events(),
                self.services.notification_service.clone(),
            );
            self.tasks.lock().push(bridge_handle);
            tracing::info!("call → notification bridge started");
        } else {
            tracing::warn!("call service not ready — call ring notifications disabled this session");
        }
    }

    /// Spawn the gRPC notification-stream consumer and store its handle in the
    /// dedicated slot, aborting any previous stream first. Internal helper used
    /// by both the initial start and the transport-switch re-spawn.
    fn spawn_notification_stream(self: &Arc<Self>, user_id: crate::domain::ids::UserId) {
        let handle = crate::services::notification_stream::spawn_grpc_notification_consumer(
            self.services.grpc_pool.clone(),
            self.event_bus().clone(),
            self.services.notification_service.clone(),
            user_id,
        );
        let mut slot = self.notif_stream_handle.lock();
        if let Some(old) = slot.take() {
            old.abort();
        }
        *slot = Some(handle);
    }

    /// Re-spawn the gRPC notification stream so it re-dials over the currently
    /// active transport (anon mode toggles the pool between direct and onion).
    /// No-op before login — the stream hasn't started, and the normal
    /// post-login `start_authed_notification_streams` will bring it up on the
    /// right transport. Idempotent: aborts the previous stream before spawning,
    /// so repeated toggles never stack or leak a task.
    pub fn respawn_notification_stream(self: &Arc<Self>) {
        if !self
            .authed_notif_started
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let user_id = match self.auth_state().get().current_user_id {
            Some(id) => id,
            None => return,
        };
        self.spawn_notification_stream(user_id);
        tracing::info!("notification stream re-dialed over new transport");
    }

    /// Account-wide push delivery preference. This does not change this
    /// device's notification content, sound, or operating-system permission.
    pub async fn account_push_enabled(&self) -> Result<bool, crate::error::NetworkError> {
        let user_id = self.session_current_user_id()
            .ok_or(crate::error::NetworkError::Unauthenticated)?.to_string();
        let preferences = self.services.grpc_pool.notification_client()
            .get_preferences(Some(&user_id)).await?
            .preferences.ok_or_else(|| crate::error::NetworkError::ServerError(
                "notification preferences missing from response".into()))?;
        Ok(preferences.push_enabled)
    }

    /// Read before updating because the server replaces the whole preference
    /// record. Preserve quiet hours, type filters, and all other delivery flags.
    pub async fn set_account_push_enabled(
        &self,
        enabled: bool,
    ) -> Result<bool, crate::error::NetworkError> {
        let user_id = self.session_current_user_id()
            .ok_or(crate::error::NetworkError::Unauthenticated)?.to_string();
        let mut notification_client = self.services.grpc_pool.notification_client();
        let mut preferences = notification_client.get_preferences(Some(&user_id)).await?
            .preferences.ok_or_else(|| crate::error::NetworkError::ServerError(
                "notification preferences missing from response".into()))?;
        preferences.push_enabled = enabled;
        let saved = notification_client.update_preferences(Some(&user_id), preferences).await?
            .preferences.ok_or_else(|| crate::error::NetworkError::ServerError(
                "notification preferences missing from response".into()))?;
        Ok(saved.push_enabled)
    }

    /// Whether message notifications make a sound (desktop; default on).
    pub fn notification_sound_enabled(&self) -> bool {
        use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
        SettingsRepository::new(self.services.database.clone())
            .get_bool(keys::NOTIFICATION_SOUND)
            .ok()
            .flatten()
            .unwrap_or(true)
    }

    /// Persist the notification-sound preference and apply it now. Startup
    /// reads the same key (`app/core_services.rs`, `NOTIFICATION_SOUND`).
    pub fn set_notification_sound_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
        SettingsRepository::new(self.services.database.clone())
            .set_bool(keys::NOTIFICATION_SOUND, enabled)?;
        self.services.notification_service.set_sound_enabled(enabled);
        Ok(())
    }
}
