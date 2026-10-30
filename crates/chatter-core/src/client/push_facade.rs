use super::*;

impl ChatterClient {
    // ── Ntfy push wake-ups ──────────────────────────────────────────────
    //
    // Android's messaging foreground service retains the existing inbox loop
    // and in-process SSE listeners while backgrounded. Inbox ntfy wakes are
    // always content-free; account notifications retain their configured mode.
    // `configure_push` starts a background
    // task that subscribes to that SSE feed and, for each event, (1) emits
    // a `DomainEvent::PushWakeup` on the event bus so the UI + listeners can
    // react, and (2) pokes the inbox poller so any freshly delivered mailbox
    // messages are pulled immediately rather than waiting for the next
    // scheduled tick.

    /// Start (or replace) the self-hosted ntfy push listener for this client.
    ///
    /// * `ntfy_url` — base URL of the ntfy instance (e.g.
    ///   `https://push.example.com`). An empty string tears down any
    ///   previously-configured listener (graceful no-op if none was
    ///   running).
    ///
    /// The caller must be logged in. Server-issued account/device capabilities
    /// and independent random anonymous-mailbox capabilities select topics.
    ///
    /// Idempotent: calling twice with the same URL cancels the first task
    /// and starts a fresh one (the wake broadcast channel itself is recreated
    /// so there's no stale subscriber to drain).
    pub fn configure_push(self: &Arc<Self>, ntfy_url: &str) -> Result<(), ClientError> {
        // Always shut down the previous listener first — this is the
        // "disable push" path when `ntfy_url` is empty, and also ensures
        // a reconfigure doesn't leak tasks.
        let mut push_slot = self.push_shutdown.lock();
        if let Some(tx) = push_slot.take() {
            let _ = tx.send(());
        }

        if let Some(task) = self.mailbox_wake_task.lock().take() {
            task.abort();
        }
        let ntfy_url = ntfy_url.trim();
        if ntfy_url.is_empty() {
            // Explicit disable — forget the remembered URL so anon-mode exit
            // doesn't resurrect a listener the caller intentionally stopped.
            *self.push_url.lock() = None;
            tracing::info!("Push listener disabled (empty URL)");
            return Ok(());
        }
        // Remember the desired push URL so anon mode can suspend push on enter
        // and restore this exact config on exit.
        *self.push_url.lock() = Some(ntfy_url.to_string());

        if self.auth_state().get().current_user_id.is_some() {
            self.start_mailbox_wakes(ntfy_url.to_string());
        }
        // ntfy SSE is direct private-CA HTTPS. In Shield — including a
        // session that signed in with Shield on — only remember the URL;
        // `rebuild_transport_loops` starts the listener when Shield goes off.
        if self.anon_mode_enabled()
            || self.services.grpc_pool.transport_mode()
                == crate::network::grpc::client_pool::TransportMode::Onion
        {
            tracing::info!("Push listener deferred: Shield is on (poll-only delivery)");
            return Ok(());
        }

        let auth = self.services.auth_state.get();
        if auth.current_user_id.is_none() {
            return Err(ClientError::Validation(
                "configure_push requires an authenticated user".into(),
            ));
        }
        let push_revision = auth.session_revision;

        // Use the opaque topic the server issued in AuthResponse (persisted at
        // login), not a locally-derived chatter_{user_id}. Empty means push is
        // disabled server-side (or we haven't logged in since the field was
        // added) — nothing to subscribe to.
        let topic = {
            use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
            SettingsRepository::new(self.services.database.clone())
                .get_string(keys::PUSH_TOPIC)
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        if topic.is_empty() {
            tracing::info!("Push listener not started: no server-issued push topic");
            return Ok(());
        }

        let mut config = crate::network::streaming::push_listener::PushListenerConfig::new(
            ntfy_url.to_string(),
            topic.clone(),
        );
        // Devices behind one NAT should not retry a throttled ntfy listener
        // at exactly the same instant.
        config.reconnect.jitter_ratio = 0.2;

        // Fresh shutdown channel for this listener instance.
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
        *push_slot = Some(shutdown_tx);

        let listener = crate::network::streaming::push_listener::PushListener::new(config);
        #[cfg(feature = "onion")]
        let listener = listener.with_tor(self.services.grpc_pool.onion_tor());
        let listener = std::sync::Arc::new(listener);
        let wake_rx = listener.subscribe();
        let client = Arc::downgrade(self);

        // Task A: SSE consumer. Runs the listener's reconnect loop; exits
        // when shutdown fires.
        let listen_listener = listener.clone();
        let listen_shutdown = shutdown_rx;
        let listen_client = client.clone();
        let h_sse = tokio::spawn(async move {
            // A reconfigure racing sign-out must not open an old account's
            // subscription after teardown. Admission waits for auth transitions.
            {
                let Some(client) = listen_client.upgrade() else {
                    return;
                };
                let _lifecycle = client.auth_lifecycle_lock.lock().await;
                if !client.push_wakeup_allowed(push_revision) {
                    return;
                }
            }
            listen_listener.listen(listen_shutdown).await;
        });

        // Task B: wake-event router. Bridges the listener's broadcast into
        // the event bus + inbox-poller poke. A separate task so the SSE
        // consumer's lifetime isn't tied to the inbox provider being ready.
        let router_topic = topic.clone();
        let mut wake_rx = wake_rx;
        let h_router = tokio::spawn(async move {
            loop {
                match wake_rx.recv().await {
                    Ok(event) => {
                        let Some(client) = client.upgrade() else {
                            break;
                        };
                        if !client
                            .route_push_wakeup(push_revision, &router_topic, event.kind())
                            .await
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "Push wake-up router lagged — dropped events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Store handles so Drop / shutdown aborts them.
        {
            let mut tasks = self.tasks.lock();
            tasks.push(h_sse);
            tasks.push(h_router);
        }

        tracing::info!("Ntfy push listener configured");
        Ok(())
    }

}
