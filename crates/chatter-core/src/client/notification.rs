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
        );
        self.tasks.lock().push(handle);

        #[cfg(feature = "mesh")]
        {
            let mesh_handle =
                crate::services::notification_service::spawn_mesh_notification_listener(
                    self.mesh_subscribe(),
                    self.notifications().clone(),
                );
            self.tasks.lock().push(mesh_handle);
        }
    }

    /// Spawn the auth-gated notification streams: the gRPC message-notification
    /// stream and the call → notification bridge (`IncomingPresented` →
    /// ringing OS notification, cancelled when the ring ends). Call after login
    /// (e.g. from initial-data load). Idempotent — runs its spawns once.
    pub fn start_authed_notification_streams(self: &Arc<Self>) {
        let revision = self.auth_state().get().session_revision;
        let client = self.clone();
        tokio::spawn(async move {
            let _lifecycle = client.auth_lifecycle_lock.lock().await;
            if client.auth_state().session_is_current(revision) {
                client.start_authed_notification_streams_locked();
            }
        });
    }

    fn start_authed_notification_streams_locked(self: &Arc<Self>) {
        let auth = self.auth_state().get();
        let Some(user_id) = auth.current_user_id else {
            return;
        };
        let revision = auth.session_revision;
        let mut owner = self.authed_notif_session.lock();
        if *owner == Some(revision) {
            return;
        }
        // A retired session must be drained by logout/shutdown before restart.
        if let Some(retired) = *owner {
            let client = self.clone();
            tokio::spawn(async move {
                {
                    let _lifecycle = client.auth_lifecycle_lock.lock().await;
                    if !client.auth_state().session_is_current(revision) {
                        return;
                    }
                    client.stop_authed_notification_streams(retired).await;
                    #[cfg(feature = "media")]
                    if let Some(worker) = client.call_service().get() {
                        let _ = worker.reset_presentation_session(retired).await;
                    }
                    if client.authed_notif_session.lock().is_none() {
                        client.start_authed_notification_streams_locked();
                        return;
                    }
                }
                // Retain exact old cancellation ownership, but never keep
                // authentication serialization locked during an unbounded retry.
                #[cfg(feature = "media")]
                {
                    let bridge = client
                        .call_notification_bridge
                        .lock()
                        .as_ref()
                        .filter(|(owner, _)| *owner == retired)
                        .map(|(_, bridge)| bridge.clone());
                    if let Some(bridge) = bridge {
                        if !bridge.wait_drained().await {
                            return;
                        }
                    } else {
                        return;
                    }
                    let _lifecycle = client.auth_lifecycle_lock.lock().await;
                    if !client.auth_state().session_is_current(revision) {
                        return;
                    }
                    client.stop_authed_notification_streams(retired).await;
                    if client.authed_notif_session.lock().is_none() {
                        client.start_authed_notification_streams_locked();
                    }
                }
            });
            return;
        }
        *owner = Some(revision);
        self.authed_notif_started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // The gRPC notification stream is held in its own slot (not the `tasks`
        // bag) so an anon-mode transport switch can abort + re-spawn just this
        // loop. The call bridge owns its revision separately so its native
        // cancellation can drain reliably before the next owner starts.
        self.spawn_notification_stream(user_id);

        #[cfg(feature = "media")]
        if let Some(call_worker) = self.services.call_service.get() {
            call_worker
                .incoming_presentation_state()
                .begin_session(revision);
            let bridge =
                crate::services::call_notification_bridge::spawn_session_call_notification_bridge(
                    call_worker.for_presentation_session(revision),
                    self.services.notification_service.clone(),
                );
            *self.call_notification_bridge.lock() = Some((revision, bridge));
            tracing::info!("call → notification bridge started");
        } else {
            tracing::warn!(
                "call service not ready — call ring notifications disabled this session"
            );
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

    pub(crate) async fn stop_authed_notification_streams(&self, revision: u64) {
        // Reserve the old lifecycle until graceful cancellation completes.
        // Starting the next owner is forbidden while this slot is occupied.
        if *self.authed_notif_session.lock() != Some(revision) {
            return;
        }
        if let Some(handle) = self.notif_stream_handle.lock().take() {
            handle.abort();
        }
        #[cfg(feature = "media")]
        {
            let bridge = {
                let slot = self.call_notification_bridge.lock();
                if slot.as_ref().is_some_and(|(owner, _)| *owner == revision) {
                    slot.as_ref().map(|(_, bridge)| bridge.clone())
                } else {
                    None
                }
            };
            if let Some(bridge) = bridge {
                if !bridge.stop().await {
                    tracing::error!(
                        revision,
                        "call notification drain failed; retaining session ownership"
                    );
                    return;
                }
            }
            let mut slot = self.call_notification_bridge.lock();
            if slot.as_ref().is_some_and(|(owner, _)| *owner == revision) {
                slot.take();
            }
        }
        let mut owner = self.authed_notif_session.lock();
        if *owner != Some(revision) {
            return;
        }
        if let Some(handle) = self.notif_stream_handle.lock().take() {
            handle.abort();
        }
        *owner = None;
        self.authed_notif_started
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Re-spawn the gRPC notification stream so it re-dials over the currently
    /// active transport (anon mode toggles the pool between direct and onion).
    /// No-op before login — the stream hasn't started, and the normal
    /// post-login `start_authed_notification_streams` will bring it up on the
    /// right transport. Idempotent: aborts the previous stream before spawning,
    /// so repeated toggles never stack or leak a task.
    pub fn respawn_notification_stream(self: &Arc<Self>) {
        let owner = self.authed_notif_session.lock();
        if *owner != Some(self.auth_state().get().session_revision) {
            return;
        }
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
        self.session_current_user_id()
            .ok_or(crate::error::NetworkError::Unauthenticated)?;
        let preferences = self
            .services
            .grpc_pool
            .notification_client()
            .get_preferences()
            .await?
            .preferences
            .ok_or_else(|| {
                crate::error::NetworkError::ServerError(
                    "notification preferences missing from response".into(),
                )
            })?;
        Ok(preferences.push_enabled)
    }

    /// Read before updating because the server replaces the whole preference
    /// record. Preserve quiet hours, type filters, and all other delivery flags.
    pub async fn set_account_push_enabled(
        &self,
        enabled: bool,
    ) -> Result<bool, crate::error::NetworkError> {
        self.session_current_user_id()
            .ok_or(crate::error::NetworkError::Unauthenticated)?;
        let mut notification_client = self.services.grpc_pool.notification_client();
        let mut preferences = notification_client
            .get_preferences()
            .await?
            .preferences
            .ok_or_else(|| {
                crate::error::NetworkError::ServerError(
                    "notification preferences missing from response".into(),
                )
            })?;
        preferences.push_enabled = enabled;
        let saved = notification_client
            .update_preferences(preferences)
            .await?
            .preferences
            .ok_or_else(|| {
                crate::error::NetworkError::ServerError(
                    "notification preferences missing from response".into(),
                )
            })?;
        Ok(saved.push_enabled)
    }

    /// Whether message notifications make a sound (desktop; default on).
    pub fn notification_sound_enabled(&self) -> bool {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
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
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .set_bool(keys::NOTIFICATION_SOUND, enabled)?;
        self.services
            .notification_service
            .set_sound_enabled(enabled);
        Ok(())
    }
}

#[cfg(all(test, feature = "media"))]
mod session_tests {
    use super::*;
    use crate::services::call_worker::{CallHandle, CallWorkerHandle, OrchestratedCallKind};
    use crate::system::notification_sink::NotificationSink;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Sink {
        log: Mutex<Vec<String>>,
        fails: std::sync::atomic::AtomicBool,
    }
    impl NotificationSink for Sink {
        fn show(
            &self,
            _: &str,
            _: &str,
            _: crate::domain::notification::NotificationPriority,
        ) -> Result<(), crate::error::SystemError> {
            Ok(())
        }
        fn show_incoming_call(
            &self,
            id: &str,
            _: &str,
            _: &str,
        ) -> Result<(), crate::error::SystemError> {
            self.log.lock().push(format!("show {id}"));
            Ok(())
        }
        fn cancel_incoming_call(&self, id: &str) -> Result<(), crate::error::SystemError> {
            if self.fails.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::error::SystemError::NotificationSend(
                    "fixture failure".into(),
                ));
            }
            self.log.lock().push(format!("cancel {id}"));
            Ok(())
        }
    }
    #[tokio::test]
    async fn real_client_session_stop_and_relogin_starts_one_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::system::Directories::with_base_dir(dir.path().to_owned())
            .unwrap()
            .database_path();
        std::fs::write(
            db.parent()
                .unwrap()
                .join(crate::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let sink = Arc::new(Sink::default());
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_string_lossy())
            .notification_sink(sink.clone())
            .build()
            .unwrap();
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let worker = CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ));
        assert!(client.call_service().set(worker.clone()).is_ok());
        let user = crate::domain::ids::UserId::new();
        for id in ["first", "same-user-new-session"] {
            client.auth_state().set_logged_in(user, 1, "Alice".into());
            let revision = client.auth_state().get().session_revision;
            client.start_authed_notification_streams();
            client.start_authed_notification_streams();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while *client.authed_notif_session.lock() != Some(revision) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let before = client.auth_state().get().session_revision;
            assert!(matches!(
                client.login("other", "unused", "fixture", None).await,
                Err(super::super::ClientError::AlreadyStarted)
            ));
            assert!(matches!(
                client
                    .register("other", "unused", "Other", "fixture", None, None)
                    .await,
                Err(super::super::ClientError::AlreadyStarted)
            ));
            assert_eq!(client.auth_state().get().session_revision, before);
            assert_eq!(client.auth_state().get().current_user_id, Some(user));
            let scoped = worker.for_presentation_session(revision);
            scoped.orch_present_incoming(
                CallHandle::from(id),
                OrchestratedCallKind::GroupChannel,
                user,
                "Caller".into(),
                false,
                false,
                String::new(),
                None,
                None,
                String::new(),
            );
            scoped.incoming_presentation_snapshot().await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !sink.log.lock().contains(&format!("show {id}")) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let _ = client.logout().await; // no server session; teardown still must complete
            assert!(client.authed_notif_session.lock().is_none());
            assert!(!client
                .authed_notif_started
                .load(std::sync::atomic::Ordering::SeqCst));
            assert!(client.call_notification_bridge.lock().is_none());
            assert!(client.notif_stream_handle.lock().is_none());
            client.auth_state().set_logged_out();
        }
        assert_eq!(
            *sink.log.lock(),
            [
                "show first",
                "cancel first",
                "show same-user-new-session",
                "cancel same-user-new-session"
            ]
        );
        // Permanent native errors retain cancellation ownership without
        // retaining authenticated producer or rejecting the next auth attempt.
        client.auth_state().set_logged_in(user, 1, "Alice".into());
        let retired = client.auth_state().get().session_revision;
        client.start_authed_notification_streams();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while *client.authed_notif_session.lock() != Some(retired) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        worker
            .for_presentation_session(retired)
            .orch_present_incoming(
                CallHandle::from("permanent"),
                OrchestratedCallKind::GroupChannel,
                user,
                "Caller".into(),
                false,
                false,
                String::new(),
                None,
                None,
                String::new(),
            );
        worker.incoming_presentation_snapshot().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !sink.log.lock().contains(&"show permanent".to_owned()) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        sink.fails.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(6), client.logout())
            .await
            .unwrap()
            .ok();
        assert!(client.notif_stream_handle.lock().is_none());
        assert!(client.call_notification_bridge.lock().is_some());
        assert_eq!(*client.authed_notif_session.lock(), Some(retired));
        assert!(!client.auth_state().is_logged_in());
        assert!(worker
            .incoming_presentation_snapshot()
            .await
            .unwrap()
            .presentations
            .is_empty());
        // Invalid PIN is handled before any connection attempt, proving the
        // real registration entry passes its guard despite retained cleanup.
        assert!(matches!(
            client
                .register("unused", "unused", "Unused", "fixture", None, Some("x"))
                .await,
            Err(super::super::ClientError::Validation(_))
        ));
        client
            .auth_state()
            .set_logged_in(user, 1, "New auth while cleanup retries".into());
        let next_revision = client.auth_state().get().session_revision;
        worker
            .incoming_presentation_state()
            .begin_session(next_revision);
        client.start_authed_notification_streams(); // exactly one external start
        worker
            .for_presentation_session(next_revision)
            .orch_present_incoming(
                CallHandle::from("auto-recovered"),
                OrchestratedCallKind::GroupChannel,
                user,
                "Caller".into(),
                false,
                false,
                String::new(),
                None,
                None,
                String::new(),
            );
        worker.incoming_presentation_snapshot().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        assert_eq!(*client.authed_notif_session.lock(), Some(retired));
        assert!(client.notif_stream_handle.lock().is_none());
        sink.fails.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !sink.log.lock().contains(&"show auto-recovered".to_owned()) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*client.authed_notif_session.lock(), Some(next_revision));
        assert!(client.notif_stream_handle.lock().is_some());
        assert_eq!(
            sink.log
                .lock()
                .iter()
                .filter(|entry| entry.as_str() == "show auto-recovered")
                .count(),
            1
        );
        let _ = client.logout().await;
        assert!(client.call_notification_bridge.lock().is_none());
        worker.shutdown();
    }
}
