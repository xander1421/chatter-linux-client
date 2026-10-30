use super::*;

impl ChatterClient {
    // ── Presence ─────────────────────────────────────────────────────

    /// Set presence status and broadcast to all anon DM conversations.
    pub async fn set_presence(&self, presence: crate::domain::user::PresenceStatus) {
        let revision = self.auth_state().get().session_revision;
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if self.auth_state().get().session_revision != revision {
            return;
        }
        let _publication = self.presence_controller.publication_lock.lock().await;
        self.send_presence_updates(presence).await;
    }

    /// Re-send the current status to the server only (no anon-DM sends, no
    /// local event). The server keeps presence for five minutes after each
    /// publish, so this keeps a live client distinguishable from one that died
    /// without saying Offline.
    pub(super) async fn refresh_identified_presence(
        &self,
        presence: crate::domain::user::PresenceStatus,
    ) {
        let _publication = self.presence_controller.publication_lock.lock().await;
        let proto = crate::services::presence_service::domain_presence_to_proto(presence);
        if let Err(e) = self.services.grpc_pool.user_client().update_presence(proto).await {
            tracing::debug!("Presence refresh failed: {}", e);
        }
    }

    /// Shutdown already owns the lifecycle lock and sends the final status
    /// after all session tasks have stopped.
    pub(super) async fn set_presence_for_shutdown(
        &self,
        presence: crate::domain::user::PresenceStatus,
    ) {
        let _publication = self.presence_controller.publication_lock.lock().await;
        self.send_presence_updates(presence).await;
    }

    async fn send_presence_updates(&self, presence: crate::domain::user::PresenceStatus) {
        let conversations = match self.anon_dms().list_conversations() {
            Ok(convs) => convs,
            Err(e) => {
                tracing::warn!("Failed to list conversations for presence: {}", e);
                Vec::new()
            }
        };

        let sends = conversations.iter().filter(|conv| conv.is_ready()).map(|conv| {
            let service = self.anon_dms().clone();
            let conv_id = conv.id;
            async move {
                if let Err(e) = service.send_presence_update(&conv_id, presence).await {
                    tracing::debug!("Failed to send presence update: {}", e);
                }
            }
        });

        // ── Identified presence ──────────────────────────────────────────
        // Publish our REAL online/offline status to the server via the
        // identified UserService.UpdatePresence RPC. The server fans this out
        // to our friends (friend-scoped) so they see us come online / go
        // offline. This is separate from the anon-DM E2E presence above, which
        // stays as-is. Awaited (not spawned) so it completes reliably even
        // during shutdown, before the gRPC connection is torn down.
        let mut user_client = self.services.grpc_pool.user_client();
        let proto = crate::services::presence_service::domain_presence_to_proto(presence);
        let identified = async {
            if let Err(e) = user_client.update_presence(proto).await {
                tracing::warn!("Failed to publish identified presence to server: {}", e);
            }
        };
        // Both sends belong to this publication future. Canceling it drops
        // every in-flight send before shutdown publishes Offline.
        tokio::join!(futures::future::join_all(sends), identified);

        // Our own presence never comes back to us on the user-events stream —
        // the server fans it out to our friends, not to the sender. Emit it
        // locally so the places that show it (profile strip, collapsed rail
        // avatar) track every transition and not only the ones the picker
        // makes: auto-away after idle, the return to online on activity, login
        // with a persisted manual mode.
        if let Some(me) = self.session_current_user_id() {
            let _ =
                self.services
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::UserPresenceChanged {
                        user_id: me,
                        presence,
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    });
        }
    }

    /// Record user activity (resets idle timer).
    ///
    /// Call this on any user interaction (message send, typing, etc.)
    /// to prevent automatic away status.
    pub fn touch_activity(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_activity_ms.store(now, Ordering::Relaxed);
    }

}
