use super::*;

impl CallSignalingService {
    pub fn set_pending_dm_call(&self, pending: PendingDmCall) {
        *self.pending_dm_call.lock() = Some(pending);
    }

    /// Take (consume) the pending outgoing DM call state.
    pub fn take_pending_dm_call(&self) -> Option<PendingDmCall> {
        self.pending_dm_call.lock().take()
    }

    /// Send a call offer with our ephemeral public key.
    ///
    /// The peer will respond with `send_call_answer` containing their
    /// ephemeral key, completing the X25519 key exchange.
    pub async fn send_call_offer(
        &self,
        peer_id: &UserId,
        call_id: &str,
        ephemeral_public_key: [u8; 32],
        call_type: &str,
    ) -> Result<(), CallSignalingError> {
        let content = build_call_key_offer(call_id, ephemeral_public_key, call_type);
        match self
            .message_service
            .send_friend_dm_content(peer_id, content)
            .await
        {
            Ok(_) => tracing::info!(peer_id = %peer_id, call_id, "Call offer sent"),
            // Shield, first DM to this peer: the mailbox is bootstrapping. The
            // offer is QUEUED and delivered once the secure channel is ready — a
            // moment later. Not a failure; let the call proceed (caller
            // shows "calling…", callee rings when the offer lands).
            Err(crate::services::message::MessageError::BootstrapInitiated) => {
                tracing::info!(peer_id = %peer_id, call_id, "Call offer queued during DM bootstrap — will deliver when secure channel is ready");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Send a call answer with our ephemeral public key.
    ///
    /// This completes the key exchange — both sides can now derive
    /// the shared secret for media encryption.
    pub async fn send_call_answer(
        &self,
        peer_id: &UserId,
        call_id: &str,
        ephemeral_public_key: [u8; 32],
    ) -> Result<(), CallSignalingError> {
        let content = build_call_key_answer(call_id, ephemeral_public_key);
        self.message_service
            .send_friend_dm_content(peer_id, content)
            .await?;
        tracing::info!(peer_id = %peer_id, call_id, "Call answer sent");
        Ok(())
    }

    /// Send a call hangup notification to the DM peer.
    ///
    /// A DM call has no server channel: its local "channel_id" IS the peer's
    /// user_id.
    pub async fn send_call_hangup(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        reason: CallHangupReason,
    ) -> Result<(), CallSignalingError> {
        let peer_id = UserId::from_uuid(*channel_id.as_uuid());
        self.send_call_hangup_to(&peer_id, call_id, reason).await
    }

    /// Tell the caller we declined their 1:1 call. The self-sync copy of this
    /// hangup stops our other devices ringing (`build_call_self_sync`).
    pub async fn send_call_decline(
        &self,
        peer_id: &UserId,
        call_id: &str,
    ) -> Result<(), CallSignalingError> {
        self.send_call_hangup_to(peer_id, call_id, CallHangupReason::Declined)
            .await
    }

    pub(super) async fn send_call_hangup_to(
        &self,
        peer_id: &UserId,
        call_id: &str,
        reason: CallHangupReason,
    ) -> Result<(), CallSignalingError> {
        let content = build_call_hangup_content(call_id, reason);
        match self
            .message_service
            .send_friend_dm_content(peer_id, content)
            .await
        {
            Ok(_) => {
                tracing::info!(peer_id = %peer_id, call_id, reason = ?reason, "Call hangup sent")
            }
            // Session still bootstrapping — the queued hangup will deliver, and the
            // peer sees the LiveKit disconnect regardless. Not fatal.
            Err(crate::services::message::MessageError::BootstrapInitiated) => {
                tracing::info!(peer_id = %peer_id, call_id, "Call hangup queued during DM bootstrap");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}
