//! Notification listener wiring on `ChatterClient`.
//!
//! Lives outside `jarvis.rs` because the listener spans device/call/session
//! events independent of the jarvis feature.

use std::sync::Arc;

use super::ChatterClient;
use crate::services::notification_service::{is_private_notification, should_notify_message};

impl ChatterClient {
    /// Bring up call services when a call is placed or an incoming ring arrives.
    /// A cold-started push can reach the inbox before notification setup runs.
    #[cfg(feature = "media")]
    pub(crate) fn ensure_call_service(&self) {
        let auth = self.auth_state().get();
        let revision = auth.current_user_id.map(|_| auth.session_revision);
        self.ctx.init_call_service(revision);
        let Some(revision) = revision else { return };
        let Some(worker) = self.call_service().get() else { return };
        worker.incoming_presentation_state().begin_session(revision);

        // The authenticated notification stream may have started while no
        // call worker existed. Attach its ring bridge to this same session.
        let owner = self.authed_notif_session.lock();
        if *owner != Some(revision) {
            return;
        }
        let mut bridge = self.call_notification_bridge.lock();
        if bridge.is_none() {
            *bridge = Some((
                revision,
                crate::services::call_notification_bridge::spawn_session_call_notification_bridge(
                    worker.for_presentation_session(revision),
                    self.services.notification_service.clone(),
                ),
            ));
            tracing::info!("call → notification bridge started");
        }
    }

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
        let handle = spawn_notification_listener(
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
            let phase = super::lifecycle_diagnostics::Phase::NotificationStart;
            phase.requested();
            let _lifecycle = client.auth_lifecycle_lock.lock().await;
            let _scope = phase.acquired();
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
                    let phase = super::lifecycle_diagnostics::Phase::NotificationRetire;
                    phase.requested();
                    let _lifecycle = client.auth_lifecycle_lock.lock().await;
                    let _scope = phase.acquired();
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
                    let phase = super::lifecycle_diagnostics::Phase::NotificationRetire;
                    phase.requested();
                    let _lifecycle = client.auth_lifecycle_lock.lock().await;
                    let _scope = phase.acquired();
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

/// Spawn a task that listens to the domain event bus and dispatches user-
/// visible notifications via `NotificationService`.
///
/// Filter policy lives in services (not in the UI / client facade) — see
/// `services::notification_service::should_notify_message`.
/// * Show reactions from other users as low-priority notifications.
///
/// "Who am I" comes from the settings-backed `UserService::current_user_id`,
/// NOT `AuthState`: the device-link bring-up never calls
/// `AuthState::set_logged_in`, so on a freshly linked device `AuthState` says
/// nobody is logged in and every self-synced copy of our own send (SyncSent)
/// used to raise a notification (F11).
///
/// Task exits when the event bus closes; returns the `JoinHandle` for the
/// caller to track alongside other lifecycle tasks.
pub(crate) fn spawn_notification_listener(
    event_bus: crate::events::EventBus,
    // Weak: the client owns this task, so a strong handle would keep the
    // client alive forever. Used for the per-conversation rules — display
    // name in context, mute, what is on screen.
    client: std::sync::Weak<crate::client::ChatterClient>,
    user_service: std::sync::Arc<crate::services::user::UserService>,
    notification_service: std::sync::Arc<crate::services::notification_service::NotificationService>,
) -> tokio::task::JoinHandle<()> {
    use crate::domain::notification::NotificationPriority;
    use crate::inbox::handlers::DomainEvent;

    let mut domain_rx = event_bus.subscribe_domain();
    tokio::spawn(async move {
        loop {
            let event = match domain_rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Notification listener lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            let me = user_service.current_user_id().ok();

            match &event {
                DomainEvent::MessageReceived { message } => {
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    if let Err(error) =
                        crate::client::conversation::ConversationHandle::mark_received_as_read(
                            &client, message,
                        )
                        .await
                    {
                        tracing::warn!(%error, "Failed to mark visible incoming message read");
                    }
                    let (on_screen, muted) = conversation_attention(&client, message);
                    if !should_notify_message(message, me.as_ref(), on_screen, muted)
                        || client.do_not_disturb()
                    {
                        continue;
                    }
                    use crate::domain::channel::NotificationPreference;
                    match conversation_preference(&client, message) {
                        NotificationPreference::None => continue,
                        NotificationPreference::Mentions
                            if !me.as_ref().is_some_and(|id| client.message_mentions_user(message, id)) => continue,
                        _ => {}
                    }

                    // In an anon group the per-group name, never the global
                    // profile name of a contact who happens to be in it.
                    let sender_name = message.sender_id.map(|id| {
                        client.display_name_in(
                            &message.conversation_type,
                            &message.conversation_id,
                            &id,
                        )
                    });
                    let preview = message.as_text();

                    let _ = notification_service.show_tracked_message_notification(
                        message.id,
                        &message.conversation_id,
                        sender_name.as_deref(),
                        preview,
                        is_private_notification(message),
                        NotificationPriority::Normal,
                    );
                }
                DomainEvent::AnonDmRequestReceived {
                    conversation_id,
                    sender_id,
                    ..
                } => {
                    if me.is_none() || Some(*sender_id) == me {
                        continue;
                    }
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    let id = (*conversation_id).into();
                    let on_screen =
                        client.chat_state().is_app_visible() && client.is_showing_conversation(id);
                    let muted = client
                        .is_conversation_muted(crate::state::ActiveConversation::AnonDm(
                            *conversation_id,
                        ))
                        .unwrap_or(false);
                    if on_screen || muted || client.do_not_disturb() {
                        continue;
                    }
                    // Requests reveal no more than an incoming secret message,
                    // including when the user allows normal message previews.
                    if let Err(error) = notification_service.show_message_notification(
                        &id,
                        None,
                        None,
                        true,
                        NotificationPriority::Normal,
                    ) {
                        tracing::warn!(%error, "Secret request notification failed");
                    }
                }
                DomainEvent::ReactionAdded {
                    scope,
                    sender_id,
                    sent_at_ns,
                    emoji,
                    reactor_id,
                    ..
                } => {
                    if me.is_none() || Some(*reactor_id) == me {
                        continue;
                    }
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    // Same mute / on-screen rules as the message it reacts
                    // to. A target we do not hold names no conversation to
                    // check, so it stays silent.
                    let Some(target) = client
                        .messages()
                        .message_repo
                        .get_by_reaction_scope(scope, sender_id, *sent_at_ns)
                        .ok()
                        .flatten()
                    else {
                        continue;
                    };
                    let (on_screen, muted) = conversation_attention(&client, &target);
                    if on_screen || muted || client.do_not_disturb() || conversation_preference(&client, &target) != crate::domain::channel::NotificationPreference::All {
                        continue;
                    }
                    let reaction = chatter_i18n::trf("Reacted {}", &[&emoji]);
                    let _ = notification_service.show_message_notification(
                        &target.conversation_id,
                        None,
                        Some(&reaction),
                        // The reaction lives in the target's conversation.
                        is_private_notification(&target),
                        NotificationPriority::Low,
                    );
                }

                // Read here or on another of our devices: the notification is
                // stale (Signal cancels a thread's notification either way).
                DomainEvent::ConversationsRead {
                    conversation_ids, ..
                } => {
                    for id in conversation_ids {
                        let _ = notification_service.cancel_conversation(id);
                    }
                }

                DomainEvent::MessageDeleted { message_id, .. } => {
                    notification_service
                        .remove_message_notifications(std::slice::from_ref(message_id));
                }

                // ================================================================
                // Typed lifecycle events (from gRPC notification stream).
                // Side-effects run in spawned tasks so the event loop stays hot.
                // ================================================================
                DomainEvent::DeviceLinked {
                    device_id,
                    approved_by_self,
                    ..
                } => {
                    // Skip the toast on the device that just approved the
                    // pairing — the UI there already showed a confirm
                    // screen. Only fan-out to siblings raises a toast.
                    if *approved_by_self {
                        continue;
                    }
                    // This toast can remain visible after the app locks.
                    let body = chatter_i18n::tr("A new device was just linked to your account.");
                    tracing::info!(device_id, "DeviceLinked notification → surfacing toast");
                    let title = chatter_i18n::tr("New device linked");
                    let _ = notification_service.show_message(
                        &title,
                        &body,
                        NotificationPriority::High,
                    );
                }

                DomainEvent::PeerDevicesChanged {
                    peer_user_id,
                    device_id,
                    change_type,
                } => {
                    // Per Signal: an added device of a staying member needs
                    // only our SKDM, a removed one no key reset — see
                    // `MessageService::handle_peer_device_change`.
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    let messages = client.messages().clone();
                    let (peer, device, change) = (*peer_user_id, *device_id, change_type.clone());
                    tokio::spawn(async move {
                        messages
                            .handle_peer_device_change(peer, device, &change)
                            .await;
                    });
                }

                _ => {}
            }
        }
    })
}

/// `(on_screen, muted)` for the conversation `message` belongs to. On screen
/// is the same "is the user looking at this" rule that marks messages read:
/// the app is visible AND `is_showing` (which also matches a mailbox DM in
/// its peer's DM view — the old `is_for_channel` check notified for those).
fn conversation_attention(
    client: &std::sync::Arc<crate::client::ChatterClient>,
    message: &crate::domain::message::Message,
) -> (bool, bool) {
    let on_screen = client.chat_state().is_app_visible() && client.is_showing(message);
    let conversation = client.classify_active_conversation(
        &message.conversation_id,
        &message.conversation_type,
        message.sender_id,
    );
    let muted = client.is_conversation_muted(conversation).unwrap_or(false);
    (on_screen, muted)
}

fn conversation_preference(
    client: &std::sync::Arc<crate::client::ChatterClient>,
    message: &crate::domain::message::Message,
) -> crate::domain::channel::NotificationPreference {
    let conversation = client.classify_active_conversation(
        &message.conversation_id,
        &message.conversation_type,
        message.sender_id,
    );
    client.conversation_notification_preference(conversation).unwrap_or_default()
}
