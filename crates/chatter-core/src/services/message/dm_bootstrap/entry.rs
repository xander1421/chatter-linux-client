use super::*;

impl MessageService {
    pub(super) fn bootstrap_mailbox_client(
        &self,
        grpc_pool: &GrpcClientPool,
    ) -> Result<AnonymousMailboxClient, MessageError> {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            grpc_pool
                .secret_mailbox_client()
                .map_err(MessageError::Network)
        } else {
            Ok(grpc_pool.anonymous_mailbox_client())
        }
    }

    /// Handle DM bootstrap when conversation is not ready.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dm_bootstrap(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        grpc_pool: Option<&Arc<GrpcClientPool>>,
        attachment_ids: Option<Vec<AttachmentId>>,
        channel_id: Option<ChannelId>,
    ) -> Result<(), MessageError> {
        // Typing is stale by the time a mailbox is ready. A receipt is still
        // meaningful then, so it must take the same queued bootstrap path as
        // other content.
        if super::dm::is_ephemeral_content(&content) {
            tracing::debug!(
                peer_id = %peer_id,
                "Dropping typing indicator - bootstrap not complete"
            );
            return Err(MessageError::BootstrapInitiated);
        }

        // A first-contact user post can wait for the independent refill and
        // retry schedules. Do this before any submission claim, key lookup or
        // bootstrap side effect: retries of this row have not reached a peer.
        if grpc_pool.is_some()
            && self
                .token_manager
                .available_token_count()
                .map_err(|e| MessageError::DmError(e.to_string()))?
                == 0
        {
            // Preserve the stronger first-contact privacy refusal when no
            // device is cached. In Shield this is strictly local, never an
            // identified device fetch, regardless of the token pool state.
            if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
                self.bootstrap_devices(auth_client, peer_id).await?;
            }
            return Err(MessageError::ShieldNotReady);
        }

        // Get our user ID for Sesame deterministic initiator selection
        let our_user_id = self.settings_repo.current_user_id()?;

        // Sesame algorithm: lower UUID initiates bootstrap, higher UUID waits
        let we_are_initiator = our_user_id.as_uuid() < peer_id.as_uuid();

        if !we_are_initiator {
            return self
                .handle_dm_bootstrap_responder(
                    auth_client,
                    peer_id,
                    content,
                    grpc_pool,
                    attachment_ids,
                    channel_id,
                    &our_user_id,
                )
                .await;
        }

        // We're the lower UUID - we are the rightful initiator
        tracing::info!(
            peer_id = %peer_id,
            our_id = %our_user_id,
            "Sesame: We're lower UUID - initiating DM bootstrap"
        );

        // No pool is the post-bootstrap flush: nothing is sent, the message
        // is re-queued behind the bootstrap still in progress.
        let Some(grpc_pool) = grpc_pool else {
            crate::services::gateway::submission::mark_submission_started()?;
            let _ = self.pending_dms.queue(
                peer_id,
                content.encode_to_vec(),
                attachment_ids.unwrap_or_default(),
                channel_id,
            );
            return Err(MessageError::BootstrapInitiated);
        };

        // Before a mailbox is made or a token spent: is any device reachable?
        let devices = self.bootstrap_devices(auth_client, peer_id).await?;

        crate::services::gateway::submission::mark_submission_started()?;
        let dm_bootstrap = DmBootstrap::new(self.database.clone());
        let mut anon_mailbox_client = self.bootstrap_mailbox_client(grpc_pool)?;

        match dm_bootstrap
            .initiate_dm(*peer_id, &mut anon_mailbox_client)
            .await
        {
            Ok(bootstrap_result) => {
                self.send_bootstrap_payload(
                    auth_client,
                    peer_id,
                    &devices,
                    self.bootstrap_delivery(),
                    &bootstrap_result.bootstrap_message,
                    grpc_pool,
                )
                .await?;

                // Queue the original message to be sent once bootstrap completes
                let content_bytes = content.encode_to_vec();
                if let Err(e) = self.pending_dms.queue(
                    peer_id,
                    content_bytes,
                    attachment_ids.unwrap_or_default(),
                    channel_id,
                ) {
                    tracing::warn!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to queue message for post-bootstrap delivery"
                    );
                } else {
                    tracing::info!(
                        peer_id = %peer_id,
                        "Message queued for delivery after bootstrap completes"
                    );
                }

                Err(MessageError::BootstrapInitiated)
            }
            Err(crate::error::MailboxError::ConversationExists) => {
                // Another bootstrap with this peer is in flight - wait for it
                let content_bytes = content.encode_to_vec();
                let _ = self.pending_dms.queue(
                    peer_id,
                    content_bytes,
                    attachment_ids.unwrap_or_default(),
                    channel_id,
                );
                tracing::info!(peer_id = %peer_id, "Message queued - waiting for key exchange");
                Err(MessageError::BootstrapInitiated)
            }
            Err(e) => {
                tracing::error!(peer_id = %peer_id, error = %e, "Failed to initiate DM bootstrap");
                Err(MessageError::DmError(format!("Bootstrap failed: {}", e)))
            }
        }
    }

    /// Handle DM bootstrap as the responder (higher UUID).
    #[allow(clippy::too_many_arguments)]
    async fn handle_dm_bootstrap_responder(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        grpc_pool: Option<&Arc<GrpcClientPool>>,
        attachment_ids: Option<Vec<AttachmentId>>,
        channel_id: Option<ChannelId>,
        our_user_id: &UserId,
    ) -> Result<(), MessageError> {
        // Acquire bootstrap lock EARLY to prevent concurrent REQUEST sends
        if !try_acquire_bootstrap_lock(peer_id) {
            tracing::debug!(
                peer_id = %peer_id,
                "BOOTSTRAP[1/4] Skipping - bootstrap already in progress for this peer"
            );
            // Still queue the message even if we skip the REQUEST
            crate::services::gateway::submission::mark_submission_started()?;
            let content_bytes = content.encode_to_vec();
            let _ = self.pending_dms.queue(
                peer_id,
                content_bytes,
                attachment_ids.unwrap_or_default(),
                channel_id,
            );
            return Err(MessageError::BootstrapInitiated);
        }

        // Prompt the peer (lower UUID) to (re-)announce its mailbox. We only
        // get here when we cannot send, so whatever state we hold — even a
        // live mailbox of our own — the peer has not given us a reachable
        // mailbox, and it will not unless asked: waiting silently is a
        // deadlock for any peer with nothing to send.
        //
        // This must reach them before anything is queued: a REQUEST that
        // cannot go out (no cached device in Shield, empty token batch) fails
        // this send visibly instead of leaving the message queued for a
        // bootstrap that will never come. With no pool (the post-bootstrap
        // flush) nothing is sent: the message is re-queued and we keep waiting.
        if let Some(grpc_pool) = grpc_pool {
            if let Err(e) = self
                .send_bootstrap_request(auth_client, peer_id, our_user_id, grpc_pool)
                .await
            {
                release_bootstrap_lock(peer_id);
                return Err(e);
            }
        }
        // Held only across the send: a lost REQUEST or MESSAGE must not
        // block the next send's REQUEST — every queued send asks again.
        release_bootstrap_lock(peer_id);

        // Queueing retains the original content even when no REQUEST was sent.
        crate::services::gateway::submission::mark_submission_started()?;
        // Queue the message so it will be sent once bootstrap completes
        let content_bytes = content.encode_to_vec();
        if let Err(e) = self.pending_dms.queue(
            peer_id,
            content_bytes,
            attachment_ids.unwrap_or_default(),
            channel_id,
        ) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to queue message while waiting for peer bootstrap"
            );
        } else {
            tracing::info!(
                peer_id = %peer_id,
                our_id = %our_user_id,
                request_sent = grpc_pool.is_some(),
                "Message queued - will send after peer's bootstrap MESSAGE"
            );
        }

        Err(MessageError::BootstrapInitiated)
    }

    /// Send a bootstrap REQUEST to every live device of the peer (lower UUID),
    /// prompting each to announce its own mailbox.
    async fn send_bootstrap_request(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        our_user_id: &UserId,
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<(), MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            our_id = %our_user_id,
            "BOOTSTRAP[1/4] Higher UUID sending REQUEST to lower UUID peer"
        );

        let devices = self.bootstrap_devices(auth_client, peer_id).await?;
        self.send_bootstrap_payload(
            auth_client,
            peer_id,
            &devices,
            self.bootstrap_delivery(),
            &bootstrap_request(our_user_id),
            grpc_pool,
        )
        .await
        .map(|_| ())
    }
}
