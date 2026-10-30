use super::*;

impl MessageService {
    /// Deliver `content` (text, receipt, typing, reaction, edit, delete, …) to
    /// a friend over the transport the current mode allows. User posts take
    /// the same decision in `send_server_dm`, through the outbox when sealed.
    ///
    /// Shield: the ephemeral mailbox over onion. A single receipt or typing
    /// indicator over sealed-sender streaming re-creates the recipient+timing
    /// signal the mailbox exists to remove (NDSS'21 §III-IV), so there is no
    /// fallback — a mailbox failure is this send's failure.
    /// `Err(BootstrapInitiated)` means the mailbox with this friend is not
    /// established yet: the content was queued for the bootstrap flush.
    ///
    /// Normal: sealed sender, streaming, for instant delivery.
    pub async fn send_friend_dm_content(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            self.send_dm(peer_id, content, &self.mailbox_manager).await
        } else {
            self.send_sealed_dm_content(peer_id, content).await
        }
    }

    /// Devices of `peer` recorded for the DM sent at `sent_at`. An empty set
    /// must be interpreted by the caller using the row's submission status.
    pub(super) fn original_dm_devices(
        &self,
        peer: UserId,
        sent_at: u64,
    ) -> Result<Vec<u32>, MessageError> {
        let me = self.current_user_id()?;
        let devices =
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.database.clone(),
            )
            .targets_for_send(
                ChannelId::from(peer),
                ConversationType::ServerDm,
                me,
                sent_at,
            )?
            .into_iter()
            .filter(|(user, _)| *user == peer)
            .map(|(_, device)| device.0)
            .collect::<Vec<_>>();
        Ok(devices)
    }

    /// Send an arbitrary `Content` proto (reactions, typing, pin, edit,
    /// delete, receipts, or a DataMessage) to every device of `peer_id`,
    /// sealed-sender + UAK-authorized, with no server-side channel. Also
    /// copies it to our own other devices (Signal-style multi-device sync)
    /// when `own_device_copy` gives it one.
    ///
    /// If the recipient's UAK is unavailable this degrades to identified
    /// (Basic auth) delivery, which exposes us to the server as the sender.
    /// That case is loud in the logs and never silent — see
    /// `deposit_sealed_to_all_devices`.
    ///
    /// Callers that need local storage (e.g. `send_server_dm`) do it
    /// themselves after this returns — this method only handles delivery.
    async fn send_sealed_dm_content(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        // Backstop for `send_friend_dm_content`: this transport names the
        // recipient to the server, which Shield mode must never do.
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MessageError::DmError(
                "Shield mode: refusing the sealed-sender streaming transport for a DM".into(),
            ));
        }
        validate_sealed_plaintext_size(content.encoded_len())?;
        let plaintext = content.encode_to_vec();

        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        let peer_id_str = peer_id.to_string();

        // Recipient: every device. A device list that went stale is
        // reconciled and the send retried inside (Signal's 409/410).
        let peer = self
            .deposit_sealed_to_all_devices(
                &mut auth_client,
                &mut message_client,
                &peer_id_str,
                &plaintext,
                None,
            )
            .await;

        if peer.delivered == 0 {
            if let Some(error) = peer.error {
                return Err(error);
            }
            return Err(MessageError::DmError(format!(
                "Failed to deliver sealed DM to any device of {peer_id}"
            )));
        }

        // What a peer device's DecryptionErrorMessage will name if it cannot
        // decrypt this (`resend_dm_on_decryption_error`).
        self.record_dm_send(*peer_id, &content, &peer.deposits);

        // Self-sync: every one of our own devices except this one.
        // One policy for both DM transports (`sync_sent_to_own_devices`).
        let own_delivered = self
            .sync_sent_to_own_devices(&mut auth_client, peer_id, &content)
            .await;

        // Surface degradation at the top of the send, not just at the deposit
        // site: this is the one place that knows the whole message failed to be
        // sealed, and it must not look like an ordinary successful send.
        if peer.degraded > 0 {
            tracing::warn!(
                peer_id = %peer_id,
                peer_devices_degraded = peer.degraded,
                "SEALED SENDER DEGRADED: sealed DM was delivered identified on some \
                 devices — the server recorded us as the sender for those"
            );
        }

        tracing::debug!(
            peer_id = %peer_id,
            peer_devices_delivered = peer.delivered,
            self_devices_delivered = own_delivered,
            "Sealed DM delivery complete"
        );

        Ok(())
    }
}
