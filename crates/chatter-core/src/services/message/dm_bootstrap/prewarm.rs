use super::*;

impl MessageService {
    /// Proactively establish the DM Signal session with a peer ("prewarm").
    ///
    /// Friendship (friend-code accept) does NOT establish the per-peer DM
    /// session/mailbox — that is bootstrapped lazily on the first DM. Because a
    /// call offer is sent as a DM, the very first call between two new friends
    /// would otherwise trigger a first-contact Sesame bootstrap, queue the
    /// offer, and fail to ring the peer. Calling this when a friendship forms
    /// moves that one-time handshake off the call path.
    ///
    /// Idempotent and safe to call from both sides of a new friendship:
    /// - already-ready conversation → no-op;
    /// - per Sesame, only the lower-UUID peer initiates; the higher-UUID peer
    ///   returns immediately and establishes its half when the incoming
    ///   bootstrap REQUEST arrives via the inbox.
    pub async fn prewarm_dm_session(&self, peer_id: &UserId) -> Result<(), MessageError> {
        // Already established? Nothing to do.
        let conversation = crate::mailbox::dm::DmConversationManager::new(self.database.clone())
            .load_conversation(peer_id)
            .map_err(|e| MessageError::DmError(format!("Failed to load conversation: {}", e)))?;
        if conversation
            .as_ref()
            .map(|c| c.is_ready_to_send())
            .unwrap_or(false)
        {
            tracing::debug!(peer_id = %peer_id, "prewarm_dm_session: already established");
            return Ok(());
        }

        // Sesame: only the lower UUID initiates. The higher UUID waits for the
        // incoming REQUEST, so prewarming from that side is a no-op.
        let our_user_id = self.settings_repo.current_user_id()?;
        if our_user_id.as_uuid() >= peer_id.as_uuid() {
            tracing::debug!(peer_id = %peer_id, "prewarm_dm_session: higher UUID — waiting for peer to initiate");
            return Ok(());
        }

        // Don't race an in-flight bootstrap (e.g. a concurrent first message).
        if !try_acquire_bootstrap_lock(peer_id) {
            tracing::debug!(peer_id = %peer_id, "prewarm_dm_session: bootstrap already in progress");
            return Ok(());
        }
        let result = self.prewarm_dm_session_initiator(peer_id).await;
        release_bootstrap_lock(peer_id);
        result
    }

    /// Initiator (lower-UUID) half of [`prewarm_dm_session`]: create our
    /// mailbox and send the bootstrap REQUEST, with no message queued.
    async fn prewarm_dm_session_initiator(&self, peer_id: &UserId) -> Result<(), MessageError> {
        let grpc_pool = &self.grpc_pool;
        let mut auth_client = self.auth_client();
        let devices = self.bootstrap_devices(&mut auth_client, peer_id).await?;
        crate::services::gateway::submission::mark_submission_started()?;
        let dm_bootstrap = DmBootstrap::new(self.database.clone());
        let mut anon_mailbox_client = self.bootstrap_mailbox_client(grpc_pool)?;

        match dm_bootstrap
            .initiate_dm(*peer_id, &mut anon_mailbox_client)
            .await
        {
            Ok(bootstrap_result) => {
                self.send_bootstrap_payload(
                    &mut auth_client,
                    peer_id,
                    &devices,
                    self.bootstrap_delivery(),
                    &bootstrap_result.bootstrap_message,
                    grpc_pool,
                )
                .await?;
                tracing::info!(peer_id = %peer_id, "prewarm_dm_session: bootstrap REQUEST sent");
                Ok(())
            }
            // Conversation already exists (another path beat us to it) — fine.
            Err(crate::error::MailboxError::ConversationExists) => {
                tracing::debug!(peer_id = %peer_id, "prewarm_dm_session: conversation already exists");
                Ok(())
            }
            Err(e) => Err(MessageError::DmError(format!(
                "prewarm bootstrap failed: {e}"
            ))),
        }
    }
}
