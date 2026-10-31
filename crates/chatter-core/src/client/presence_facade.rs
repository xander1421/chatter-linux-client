use super::*;

impl ChatterClient {
    // ── Presence ─────────────────────────────────────────────────────

    /// Admission is the only lifecycle critical section. Cancellation owns the
    /// entire lock wait + operation; dropping it releases policy/network futures
    /// before the RAII publication registration is removed.
    pub(super) async fn with_presence_publication<F, Fut>(
        &self,
        revision: u64,
        operation: F,
    ) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        tracing::info!(target: "call_setup::presence_requested", "Presence publication diagnostic");
        let publication = {
            let _lifecycle = self.auth_lifecycle_lock.lock().await;
            if !self.auth_state().session_is_current(revision) {
                tracing::info!(target: "call_setup::presence_rejected", "Presence publication diagnostic");
                return false;
            }
            let Some(publication) = self.presence_controller.publications.admit(revision) else {
                tracing::info!(target: "call_setup::presence_rejected", "Presence publication diagnostic");
                return false;
            };
            publication
        };
        tracing::info!(target: "call_setup::presence_admitted", "Presence publication diagnostic");
        let mut observation = super::presence_owner::Observation::new();
        let work = async {
            let _turn = self.presence_controller.publication_lock.lock().await;
            if !self.auth_state().session_is_current(revision) {
                return false;
            }
            tracing::info!(target: "call_setup::presence_publication_acquired", "Presence publication diagnostic");
            operation().await;
            true
        };
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = publication.cancel.cancelled() => false,
            completed = &mut work => {
                if completed { observation.complete(); }
                completed
            }
        }
    }

    /// Ordinary startup/manual publication, fenced to its captured session.
    pub async fn set_presence(&self, presence: crate::domain::user::PresenceStatus) {
        let revision = self.auth_state().get().session_revision;
        self.with_presence_publication(revision, || self.send_presence_updates(presence))
            .await;
    }

    /// Refresh reads policy only after acquiring the publication turn: a queued
    /// old Online snapshot cannot overwrite a newer manual Offline/DND choice.
    pub(super) async fn refresh_identified_presence(&self, revision: u64) {
        self.with_presence_publication(revision, || async {
            let presence = self.current_presence_for_publication().await;
            if presence == crate::domain::user::PresenceStatus::Offline {
                return;
            }
            self.send_identified_presence(presence).await;
        })
        .await;
    }

    /// Caller holds lifecycle and still owns the retiring credentials. Ordinary
    /// admission stays closed until a genuinely new authenticated revision.
    pub(super) async fn set_presence_for_shutdown(
        &self,
        presence: crate::domain::user::PresenceStatus,
    ) {
        self.with_final_presence(|| self.send_presence_updates(presence))
            .await;
    }

    /// A failed/cancelled deletion resumes the same owner; successful deletion
    /// commits the pause before changing session state. Caller holds lifecycle.
    pub(super) async fn suspend_presence(
        &self,
        revision: u64,
    ) -> super::presence_owner::Suspension<'_> {
        let suspension = super::presence_owner::Suspension {
            client: self,
            revision,
            generation: self.auth().auth_interceptor.generation(),
            resume: true,
        };
        self.presence_controller.publications.close(revision);
        self.presence_controller.publications.drain().await;
        if self.signal().actor.drain_submitted().await.is_err() {
            tracing::warn!(target: "call_setup::presence_crypto_drain_failed", "Presence publication diagnostic");
        }
        suspension
    }

    /// Caller holds lifecycle. The ordinary owner is closed/drained before the
    /// final publication; its network future finishes before credentials change.
    pub(super) async fn with_final_presence<F, Fut>(&self, operation: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        self.retire_presence().await;
        let _publication = self.presence_controller.publication_lock.lock().await;
        operation().await;
    }

    async fn send_identified_presence(&self, presence: crate::domain::user::PresenceStatus) {
        #[cfg(test)]
        {
            let send = self.presence_controller.test_send.lock().clone();
            if let Some(send) = send {
                send(presence, true).await;
                return;
            }
        }
        let proto = crate::services::presence_service::domain_presence_to_proto(presence);
        if let Err(e) = self
            .services
            .grpc_pool
            .user_client()
            .update_presence(proto)
            .await
        {
            tracing::warn!("Failed to publish identified presence to server: {}", e);
        }
    }

    pub(super) async fn send_presence_updates(
        &self,
        presence: crate::domain::user::PresenceStatus,
    ) {
        #[cfg(test)]
        {
            let send = self.presence_controller.test_send.lock().clone();
            if let Some(send) = send {
                send(presence, false).await;
                return;
            }
        }
        let owner = self.auth_state().get();
        let conversations = match self.anon_dms().list_conversations() {
            Ok(convs) => convs,
            Err(e) => {
                tracing::warn!("Failed to list conversations for presence: {}", e);
                Vec::new()
            }
        };

        let sends = conversations
            .iter()
            .filter(|conv| conv.is_ready())
            .map(|conv| {
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
        let identified = self.send_identified_presence(presence);
        // Both sends belong to this publication future. Canceling it drops
        // every in-flight send before shutdown publishes Offline.
        tokio::join!(futures::future::join_all(sends), identified);

        // Our own presence never comes back to us on the user-events stream —
        // the server fans it out to our friends, not to the sender. Emit it
        // locally so the places that show it (profile strip, collapsed rail
        // avatar) track every transition and not only the ones the picker
        // makes: auto-away after idle, the return to online on activity, login
        // with a persisted manual mode.
        if let Some(me) = owner
            .current_user_id
            .filter(|_| self.auth_state().session_is_current(owner.session_revision))
        {
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
