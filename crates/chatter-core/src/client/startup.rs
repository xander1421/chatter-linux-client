use super::*;

impl ChatterClient {
    /// Warm Tor directory/guard state when explicitly requested. This
    /// contacts Tor relays only, never the Chatter service; secret and Shield
    /// clients still derive their isolated circuits at first use. Explicit
    /// startup keeps SDK construction and offline tests free of network work.
    pub fn prewarm_tor(&self) {
        #[cfg(feature = "onion")]
        crate::network::grpc::onion_transport::prewarm(self.services.grpc_pool.onion_tor());
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    /// Connect to the server (establish gRPC channel).
    ///
    /// The gRPC pool is created at `build()` but connects lazily.
    /// This forces an immediate connection attempt to verify reachability.
    /// Optional — RPCs will auto-connect on first use if skipped.
    ///
    /// Mirrors authoritative reachability into `offline_state`, including newer
    /// RPC evidence that superseded this dial. Dial errors remain caller errors;
    /// a rejected stale outcome never independently changes the displayed state.
    pub async fn connect(&self) -> Result<(), ClientError> {
        // First contact after a signed-out start: the user has acted, on the
        // transport they chose, so the reachability monitor may run now (it
        // never probes while the pool is on onion).
        if self.server_contact_deferred.swap(false, Ordering::SeqCst) {
            self.start_server_monitor();
        }
        let result = self.services.grpc_pool.connect().await;
        crate::network::server_monitor::mirror(self.offline_state());
        result.map_err(Into::into)
    }

    /// Start all background services after authentication.
    ///
    /// Call AFTER login/register. Call `subscribe_events()` BEFORE this
    /// to avoid missing events.
    ///
    /// This initializes:
    /// - Post-login infrastructure (device inbox, group fanout)
    /// - Event handlers (DomainEvent → state store + DB persistence)
    /// - Inbox polling + protocol processor
    /// - Channel event stream, bootstrap, DM bootstrap
    /// - AI services (Jarvis, local AI)
    /// - Presence (broadcasts online status)
    /// Multi-device crypto maintenance listener. One task that:
    ///   * `SenderKeyReceived` → replays any deferred group decrypts whose SKDM
    ///     finally arrived.
    ///   * `ChannelMemberLeft` (someone else) → marks the channel for sender-key
    ///     rotation so the next send issues a new key the ex-member can't read.
    ///     Skips our own departure — `leave_channel` already rotated + purged.
    fn spawn_crypto_maintenance_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        let mut rx = self.services.event_bus.subscribe_domain();
        let own_user_id = self.services.auth_state.get().current_user_id;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(DomainEvent::SenderKeyReceived {
                        sender_id,
                        sender_device_id,
                        channel_id,
                        ..
                    }) => {
                        client
                            .messages()
                            .retry_pending_group_decrypts(channel_id, sender_id, sender_device_id)
                            .await;
                    }
                    Ok(DomainEvent::ChannelMemberLeft {
                        channel_id,
                        user_id,
                        ..
                    }) => {
                        if Some(user_id) == own_user_id {
                            continue;
                        }
                        if let Err(e) = client.signal().mark_channel_for_rotation(&channel_id) {
                            tracing::warn!(
                                channel_id = %channel_id,
                                user_id = %user_id,
                                error = %e,
                                "Failed to mark channel for rotation after member left"
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "Multi-device crypto listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Receive transports share the repository's post-commit notification.
    /// Unlike the UI broadcast, this queue cannot drop a burst of stored rows.
    fn spawn_channel_delivery_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        let mut stored = self.services.database.subscribe_message_stores();
        // A guard also reports cancellation/panic; shutdown marks the intentional abort.
        struct ExitGuard(Arc<ChatterClient>);
        impl Drop for ExitGuard {
            fn drop(&mut self) {
                if !self.0.channel_delivery_shutdown.load(Ordering::SeqCst) {
                    tracing::warn!(
                        "Channel delivery listener exited outside shutdown (receiver closed/replaced, cancellation or panic)"
                    );
                }
            }
        }
        self.channel_delivery_shutdown
            .store(false, Ordering::SeqCst);
        let guard = ExitGuard(Arc::clone(self));
        tracing::info!("Channel delivery listener subscribed");
        tokio::spawn(async move {
            let _guard = guard;
            // Recovery bounds each channel delivery attempt, including retries.
            while let Some(message) = stored.recv().await {
                if message.conversation_type != crate::domain::message::ConversationType::Channel {
                    continue;
                }
                let current = client.auth_state().get();
                let channel_id = message.conversation_id;
                let timestamp = message.sent_at;
                let started = std::time::Instant::now();
                let skip = if current.current_user_id.is_none() || current.session_ended {
                    Some("signed_out")
                } else if message.sender_id.is_none() {
                    Some("no_sender")
                } else if message.sender_id == current.current_user_id {
                    Some("own_message")
                } else if matches!(
                    message.content,
                    crate::domain::message::MessageContent::System(_)
                        | crate::domain::message::MessageContent::Call(_)
                ) {
                    Some("control_message")
                } else {
                    None
                };
                if let Some(outcome) = skip {
                    tracing::info!(%channel_id, message_id = %message.id, timestamp, outcome,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "Channel delivery dispatch");
                    continue;
                }
                tracing::info!(%channel_id, message_id = %message.id, timestamp,
                    outcome = "started", elapsed_ms = 0u64, "Channel delivery dispatch");
                let result = client
                    .conversation(ActiveConversation::Channel(channel_id))
                    .send_delivery_receipt(&[timestamp])
                    .await;
                let outcome = match &result {
                    Ok(()) => "completed_or_deduplicated",
                    Err(crate::services::conversation::ConversationError::Message(
                        crate::services::MessageError::Network(crate::error::NetworkError::Timeout),
                    )) => "timeout",
                    Err(_) => "failed",
                };
                tracing::info!(%channel_id, message_id = %message.id, timestamp, outcome,
                    elapsed_ms = started.elapsed().as_millis() as u64, "Channel delivery dispatch");
                if let Err(error) = result {
                    tracing::warn!(%channel_id, message_id = %message.id, timestamp, %error,
                        "Failed to send channel delivery receipt");
                }
            }
        })
    }

    /// Call-worker group-call key policy listener. In a channel-based group
    /// call the designated rotator rotates the encryption key on every
    /// membership change: on `ParticipantLeft` so the departed participant
    /// can't decrypt future frames, and on `ParticipantJoined` — at once,
    /// joins during a running rotation coalesced into one more — so a joiner is sent only
    /// a key that covered nothing before it joined. A received
    /// `GroupCallSignal` rings, installs a key, or — a key request we are
    /// the authority for — rotates like a join. The "when" decisions live in
    /// core, not the UI crate.
    ///
    /// The loop only dispatches: each event's network work runs in its own
    /// task with deadlines, so a slow server or a flood of keys never holds
    /// up a `ParticipantLeft`. Received keys are handled at most
    /// [`CALL_KEY_TASKS`] at a time and need no ordering: adoption only
    /// moves forward (`CallKeyState::adopt`). A signal dropped there, or lost
    /// to a domain-bus lag, triggers a resync: we ask the key authority for a
    /// fresh rotation (`resync_group_call_key`). If the room-event bus lags,
    /// the roster is re-read and any departure in the gap rotates the key.
    #[cfg(feature = "media")]
    fn spawn_call_room_policy_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        let mut domain_rx = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            // Wait for the first incoming or outgoing call to create the worker.
            let (worker, mut rx) = loop {
                if let Some(w) = client.ctx.get_call_service() {
                    let rx = w.subscribe_room_events();
                    break (w.clone(), rx);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            };

            use crate::calls::LiveKitRoomEvent;
            use std::collections::HashSet;
            use tokio::sync::broadcast::error::RecvError;
            let join_gate = Arc::new(calls::JoinRotationGate::default());
            let key_slots = Arc::new(tokio::sync::Semaphore::new(CALL_KEY_TASKS));
            let mut children = tokio::task::JoinSet::new();
            // Who we last knew to be in the room, to find departures a lag
            // swallowed. Refreshed from the worker on every state change
            // (connect, and each participant count change).
            let mut known_roster: HashSet<String> = HashSet::new();

            let departed = |identity: String, children: &mut tokio::task::JoinSet<()>| {
                let client = Arc::clone(&client);
                children.spawn(async move {
                    if let Err(e) = client.handle_participant_departed(&identity).await {
                        tracing::error!(error = %e, "Failed to handle participant departure");
                    }
                });
            };

            loop {
                tokio::select! {
                    _ = children.join_next(), if !children.is_empty() => {}
                    event = rx.recv() => match event {
                        Ok(LiveKitRoomEvent::ParticipantLeft { identity }) => {
                            known_roster.remove(&identity);
                            departed(identity, &mut children);
                        }
                        Ok(LiveKitRoomEvent::ParticipantJoined { identity, .. }) => {
                            known_roster.insert(identity);
                            schedule_join_rotation(&client, &join_gate, &mut children);
                        }
                        Ok(LiveKitRoomEvent::StateChanged(_)) => {
                            known_roster = worker.remote_participant_identities().await.into_iter().collect();
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "Call room-events policy lagged; re-reading the roster");
                            let now: HashSet<String> = worker.remote_participant_identities().await.into_iter().collect();
                            let (gone, new) = roster_changes(&known_roster, &now);
                            known_roster = now;
                            // One rotation locks out every departure in the gap.
                            if let Some(identity) = gone.into_iter().next() {
                                departed(identity, &mut children);
                            }
                            if !new.is_empty() {
                                schedule_join_rotation(&client, &join_gate, &mut children);
                            }
                        }
                        Err(RecvError::Closed) => break,
                    },
                    event = domain_rx.recv() => match event {
                        Ok(DomainEvent::GroupCallSignalReceived { call_id, sender_id, channel_id, signal }) => {
                            let Ok(slot) = Arc::clone(&key_slots).try_acquire_owned() else {
                                // Dropped, so resync: whatever it was, a
                                // fresh rotation covers it.
                                tracing::warn!(%sender_id, "Group call signal dropped: handling saturated; resyncing the key");
                                client.resync_group_call_key();
                                continue;
                            };
                            let client = Arc::clone(&client);
                            let gate = Arc::clone(&join_gate);
                            children.spawn(async move {
                                // A key request due an answer rotates like a join.
                                if client.handle_group_call_signal(call_id, sender_id, channel_id, signal).await
                                    && gate.request()
                                {
                                    run_join_rotation(client, gate).await;
                                }
                                drop(slot);
                            });
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(n)) => {
                            // A lost signal may be a rotation we needed: never
                            // carry on as if nothing was missed (§124 1k).
                            tracing::warn!(skipped = n, "Call key policy lagged on domain events; resyncing the key");
                            client.resync_group_call_key();
                        }
                        Err(RecvError::Closed) => break,
                    },
                }
            }
            children.abort_all();
            while children.join_next().await.is_some() {}
        })
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), ClientError> {
        struct StartupListeners<'a> {
            handles: Vec<tokio::task::JoinHandle<()>>,
            channel_delivery_shutdown: &'a AtomicBool,
        }
        impl Drop for StartupListeners<'_> {
            fn drop(&mut self) {
                if !self.handles.is_empty() {
                    self.channel_delivery_shutdown.store(true, Ordering::SeqCst);
                }
                for handle in &self.handles {
                    handle.abort();
                }
            }
        }

        if self.started.swap(true, Ordering::SeqCst) {
            return Err(ClientError::AlreadyStarted);
        }

        // Login and register set AuthState themselves; a device that was
        // LINKED reaches here with it empty, and every reader of it (the
        // own-message notification gate among them) went blind until an app
        // restart. Fill it from the persisted identity.
        if self.auth_state().get().current_user_id.is_none() {
            let sessions = self.sessions();
            if let (Some(user_id), Some(device_id)) =
                (sessions.current_user_id(), sessions.current_device_id())
            {
                let name = self.display_name(&user_id);
                self.auth_state().set_logged_in(user_id, device_id, name);
            }
        }

        // Arm the Android background lifecycle before any network/bootstrap
        // awaits: leaving the Activity during startup must retain the poller.
        crate::system::messaging_background::set_signed_in(
            self.auth_state().get().current_user_id.is_some(),
        );

        // Activity records events from the streams started below. A
        // Service-first Android start has no UI yet, so the listener cannot
        // wait for one: pending invites re-announced on connect were lost.
        self.start_activity_listener();

        // ── Infrastructure init ─────────────────────────────────────
        self.ctx.init_device_inbox_provider();
        #[cfg(target_os = "android")]
        if let Some(provider) = self.services.device_inbox_provider.get() {
            provider.set_app_visible(self.chat_state().is_app_visible());
        }
        self.ctx.init_group_fanout();
        // ── Event handling ──────────────────────────────────────────
        let mut listeners = StartupListeners {
            handles: Vec::new(),
            channel_delivery_shutdown: &self.channel_delivery_shutdown,
        };
        listeners.handles.push(self.ctx.spawn_event_handlers());

        // ── Multi-device crypto maintenance listener ────────────────
        listeners.handles.push(self.spawn_crypto_maintenance_listener());
        listeners.handles.push(self.spawn_channel_delivery_listener());

        // ── Call worker room-events policy listener ─────────────────
        #[cfg(feature = "media")]
        listeners.handles.push(self.spawn_call_room_policy_listener());

        // ── Inbox polling + protocol processor ──────────────────────
        let inbox_rx = match self.services.start_inbox_provider().await {
            Some(rx) => rx,
            None => {
                listeners.channel_delivery_shutdown.store(true, Ordering::SeqCst);
                for handle in &listeners.handles {
                    handle.abort();
                }
                for handle in listeners.handles.drain(..) {
                    let _ = handle.await;
                }
                self.started.store(false, Ordering::SeqCst);
                return Err(ClientError::Inbox("failed to start inbox".into()));
            }
        };
        let processor = crate::services::InboxEventProcessor::from_client(self);
        let mut inbox_rx = inbox_rx;
        let h_inbox = tokio::spawn(async move {
            loop {
                match inbox_rx.recv().await {
                    Ok(event) => {
                        processor.process(&event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "InboxEventProcessor lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // ── Finish an interrupted profile-key rotation ──────────────
        {
            let client = self.clone();
            tokio::spawn(async move {
                if let Err(e) = client.finish_profile_key_rotation().await {
                    tracing::warn!(error = %e, "profile key rotation still pending");
                }
            });
        }

        // ── Channel + DM services ───────────────────────────────────
        let h_saved_sync_retry = crate::services::message::spawn_saved_sync_recovery(self);
        let h_channel_send_retry = crate::services::message::spawn_channel_send_recovery(self);
        let h_dm_send_retry = crate::services::message::spawn_dm_send_recovery(self);
        let h_delete_batch_retry = crate::services::conversation::recovery::spawn_delete_batch_recovery(self);
        let h_receipt_retry = crate::services::conversation::recovery::spawn_receipt_recovery(self);
        let h_channel_stream = self.ctx.start_channel_event_stream();
        // Live channel messages. This is the ONLY subscriber of the
        // server-managed channel message path: it re-subscribes on every
        // connect from the server's membership list, so channels joined in a
        // previous session start delivering again after a restart (INFRA-1),
        // and it runs a durable catch-up fetch on each connect so nothing
        // depends on the stream having been up.
        let h_channel_messages = self
            .services
            .channel_stream_service
            .clone()
            .start_channel_message_stream();
        let h_channel_bootstrap = self.ctx.start_channel_bootstrap_service();
        let h_dm_bootstrap = self.ctx.spawn_dm_bootstrap_handler();

        // ── AI services ─────────────────────────────────────────────
        #[cfg(feature = "jarvis")]
        let jarvis = crate::services::jarvis::JarvisBackgroundBuilder::new(self.jarvis().clone())
            .with_event_bus(self.event_bus().clone())
            .spawn();

        let h_ai = tokio::spawn({
            let local_ai = self.local_ai().clone();
            async move {
                match local_ai.load_preferred_model().await {
                    Ok(Some(info)) => {
                        tracing::info!("Auto-loaded local AI model: {}", info.model_id)
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("Failed to auto-load local AI model: {}", e),
                }
            }
        });

        // ── Pre-fetch sender certificate (needed for sealed sender / anon DM) ──
        {
            let signal = self.signal().clone();
            let grpc_pool = self.services.grpc_pool.clone();
            tokio::spawn(async move {
                let mut auth = grpc_pool.auth_client();
                match signal.get_or_request_sender_certificate(&mut auth).await {
                    Ok(_) => tracing::info!("Sender certificate pre-fetched"),
                    Err(e) => tracing::warn!("Failed to pre-fetch sender certificate: {}", e),
                }
            });
        }

        // ── Multi-device sync engine ─────────────────────────────────
        // Periodic pull of channels, messages, friends and the block list.
        // The block-list pass is load-bearing for security: the inbound block
        // filter (`services/gateway/block_filter.rs`) reads the `friendships`
        // rows this writes, so without this loop a block made on another
        // device is never enforced here.
        let h_sync = self.services.sync_service.start_background_sync();

        // ── PreKey replenishment (runs initial check immediately) ────
        let h_prekey = self.services.spawn_prekey_replenishment();

        // ── DM mailbox key rolling (L4 anonymity rotation) ───────────
        let h_key_rolling = self.services.spawn_key_rolling();

        // ── Mailbox-token refill (jittered, decoupled from sends) ────
        let h_token_refill = self.services.spawn_token_refill();
        let h_shield_retry = crate::services::message::spawn_shield_send_recovery(self);

        // ── Disappearing-messages local reaper ───────────────────────
        // Hard-deletes every message whose timer has run out, with the
        // attachments only it carried (`services::disappearing::reap_expired`),
        // and tells the UI. The first tick fires immediately, so a message that
        // expired while the app was closed goes on the next boot (expiry times
        // persist — v015). Interval overridable via
        // `CHATTER_EPHEMERAL_REAP_SECS`; default 10s — one indexed query
        // (idx_ephemeral_expires).
        let h_ephemeral_reaper = {
            let db = self.services.database.clone();
            let expired = self.expired_messages.clone();
            let notifications = self.services.notification_service.clone();
            let interval_secs = std::env::var("CHATTER_EPHEMERAL_REAP_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|s| *s > 0)
                .unwrap_or(10);
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_secs(interval_secs));
                loop {
                    ticker.tick().await;
                    match crate::services::disappearing::reap_expired(&db) {
                        Ok(gone) if !gone.is_empty() => {
                            tracing::info!(
                                count = gone.len(),
                                "ephemeral reaper: removed expired local messages"
                            );
                            notifications.remove_message_notifications(
                                &gone.iter().map(|message| message.id).collect::<Vec<_>>(),
                            );
                            // A restart clears the ledger while Android may retain its tag.
                            let message_repo = crate::storage::repositories::MessageRepository::new(db.clone());
                            for (conversation_id, conversation_type) in gone.iter()
                                .map(|m| (m.conversation_id, m.conversation_type.clone()))
                                .collect::<std::collections::HashSet<_>>()
                            {
                                match message_repo.get_unread_count(&conversation_id, &conversation_type) {
                                    Ok(0) => {
                                        if let Err(e) = notifications.cancel_conversation(&conversation_id) {
                                            tracing::warn!(error = %e, "Failed to cancel expired conversation notification");
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => tracing::warn!(error = %e, "Failed to check unread messages after expiry"),
                                }
                            }
                            let attachment_repo = crate::storage::repositories::AttachmentRepository::new(db.clone());
                            let removed_attachment_ids = gone.iter()
                                .flat_map(|message| message.attachments.iter().copied())
                                .collect::<std::collections::HashSet<_>>()
                                .into_iter()
                                .filter(|id| matches!(attachment_repo.get(id), Ok(None)))
                                .collect();
                            let _ = expired.send(ExpiredMessages { messages: gone, removed_attachment_ids });
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "ephemeral reaper: cleanup failed"),
                    }
                }
            })
        };

        // ── Presence ────────────────────────────────────────────────
        // Start without blocking the first screen, but keep the handle so
        // shutdown can stop and join the publication before sending Offline.
        let h_presence = {
            let client = self.clone();
            tokio::spawn(async move {
                client
                    .set_presence(crate::domain::user::PresenceStatus::Online)
                    .await;
            })
        };
        self.touch_activity();

        // ── Store handles for shutdown ───────────────────────────────
        let mut tasks = self.tasks.lock();
        tasks.extend(listeners.handles.drain(..));
        tasks.extend([
            h_inbox,
            h_channel_stream,
            h_delete_batch_retry,
            h_receipt_retry,
            h_saved_sync_retry,
            h_channel_send_retry,
            h_dm_send_retry,
            h_channel_messages,
            h_channel_bootstrap,
            h_dm_bootstrap,
            h_ai,
            h_sync,
            h_prekey,
            h_key_rolling,
            h_token_refill,
            h_shield_retry,
            h_ephemeral_reaper,
            h_presence,
        ]);
        #[cfg(feature = "jarvis")]
        {
            tasks.push(jarvis.maintenance);
            if let Some(observer) = jarvis.observer {
                tasks.push(observer);
            }
        }

        drop(tasks);
        // Startup after session restore, login, registration and device linking
        // all converge here after credentials/topic and inbox are ready.
        self.start_configured_push();
        Ok(())
    }

}
