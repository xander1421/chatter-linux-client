use crate::domain::UserId;
use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmStatus};
use crate::domain::private_friend_code::PrivateFriendCode;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::proto::content_v1::{self, AnonDmRequest, Content};
use crate::util::time::TimestampNanos;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Start a new anonymous DM conversation with a stranger.
    ///
    /// Creates a local conversation and sends a message request to the peer's
    /// device inbox. The conversation becomes active when they accept.
    pub async fn start_conversation(
        &self,
        peer_id: UserId,
        intro_message: Option<&str>,
    ) -> Result<AnonDm, AnonDmError> {
        let current_user = self.current_user_id()?;

        // Can't message yourself
        if peer_id == current_user {
            return Err(AnonDmError::CannotMessageSelf);
        }

        // Check if conversation already exists
        if let Some(existing) = self.anon_dm_repo.get_conversation_by_peer(&peer_id)? {
            // Allow retry if:
            // - Previous request was declined by them
            // - Our outgoing request is still pending (never got response, retrying with new code)
            let can_retry = existing.status == AnonDmStatus::Declined
                || (existing.status == AnonDmStatus::Pending && existing.initiated_by_us);

            if can_retry {
                tracing::info!(
                    old_status = ?existing.status,
                    "Removing old conversation to allow retry"
                );
                // Delete old mailbox first (if any), then conversation
                let _ = self.anon_dm_repo.delete_mailbox(&existing.id);
                self.anon_dm_repo.delete_conversation(&existing.id)?;
            } else {
                return Err(AnonDmError::ConversationExists(peer_id));
            }
        }

        // Create conversation locally
        let dm = AnonDm::new_initiated(peer_id);
        self.anon_dm_repo.insert_conversation(&dm)?;

        let sent = async {
            // Create our mailbox for this conversation
            let mailbox = self.create_conversation_mailbox(&dm.id).await?;
            self.anon_dm_repo.insert_mailbox(&mailbox)?;

            // Get profile key if available
            let profile_key = self.get_my_profile_key();

            // Send message request to peer's device inbox
            self.send_message_request(
                &peer_id,
                &dm.id,
                &mailbox.public_key,
                intro_message,
                profile_key.as_ref(),
            )
            .await
        }
        .await;
        if let Err(e) = sent {
            self.discard_if_unsent(&dm.id).await;
            return Err(e);
        }

        // Reload: sealing the request bound it to the peer's device.
        Ok(self
            .anon_dm_repo
            .get_conversation(&dm.id)?
            .ok_or(AnonDmError::ConversationNotFound(dm.id))?)
    }

    /// After a failed start: drop the conversation if its request provably
    /// never left this device (failed before the send RPC), so nothing
    /// half-made is left behind, its server mailbox included: no peer ever
    /// learned that mailbox's key, so nothing will arrive there. A request
    /// that may have left stays, with its uncertain delivery and the mailbox
    /// a reply would land in, exactly as it is.
    async fn discard_if_unsent(&self, id: &AnonDmId) {
        let unsent = matches!(
            self.anon_dm_repo.get_conversation(id),
            Ok(Some(dm)) if dm.is_unsent_request()
        );
        if !unsent {
            return;
        }
        let mailbox = self.anon_dm_repo.get_mailbox(id).ok().flatten();
        let _ = self.anon_dm_repo.delete_mailbox(id);
        if let Err(e) = self.anon_dm_repo.delete_conversation(id) {
            tracing::warn!(error = %e, "could not discard an unsent secret-chat request");
        }
        if let Some(mailbox) = mailbox
            && let Err(e) = self.delete_server_mailbox(&mailbox).await
        {
            // Best effort: the server's TTL removes it anyway.
            tracing::warn!(error = %e, "could not delete an unused secret-chat mailbox");
        }
    }

    /// Start a new anonymous DM conversation using NDSS'21-compliant flow.
    ///
    /// This is the privacy-preserving method that:
    /// 1. Uses keys from the private friend code (no server lookup)
    /// 2. Encrypts initial message with ECIES (no X3DH prekey fetch)
    /// 3. Server cannot link sender to recipient
    ///
    /// # Arguments
    ///
    /// * `friend_code` - Private friend code containing peer's identity key and delivery token
    /// * `intro_message` - Optional introduction message
    ///
    /// # Privacy Guarantees
    ///
    /// - No prekey fetch: Keys come from out-of-band friend code
    /// - No server lookup: Peer ID, identity key, delivery token all embedded
    /// - Anonymous delivery: Uses blind signature token
    /// - Sealed sender: Server can't identify sender
    pub async fn start_conversation_private(
        &self,
        friend_code: &PrivateFriendCode,
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> Result<AnonDm, AnonDmError> {
        let current_user = self.current_user_id()?;
        let peer_id = friend_code.user_id;

        // Can't message yourself
        if peer_id == current_user {
            return Err(AnonDmError::CannotMessageSelf);
        }

        // Check if conversation already exists
        if let Some(existing) = self.anon_dm_repo.get_conversation_by_peer(&peer_id)? {
            // Allow retry if:
            // - Previous request was declined by them
            // - Our outgoing request is still pending (never got response, retrying with new code)
            let can_retry = existing.status == AnonDmStatus::Declined
                || (existing.status == AnonDmStatus::Pending && existing.initiated_by_us);

            if can_retry {
                tracing::info!(
                    old_status = ?existing.status,
                    "Removing old conversation to allow retry (private mode)"
                );
                // Delete old mailbox first (if any), then conversation
                let _ = self.anon_dm_repo.delete_mailbox(&existing.id);
                self.anon_dm_repo.delete_conversation(&existing.id)?;
            } else {
                return Err(AnonDmError::ConversationExists(peer_id));
            }
        }

        // Create conversation locally
        let dm = AnonDm::new_initiated(peer_id);
        self.anon_dm_repo.insert_conversation(&dm)?;

        let sent = async {
            // Create our mailbox for this conversation
            let mailbox = self.create_conversation_mailbox(&dm.id).await?;
            self.anon_dm_repo.insert_mailbox(&mailbox)?;

            // Get profile key if available
            let profile_key = self.get_my_profile_key();

            // Send message request using ECIES (NDSS'21 compliant - no prekey fetch)
            self.send_message_request_ecies(
                friend_code,
                &dm.id,
                &mailbox.public_key,
                intro_message,
                sender_display_name,
                profile_key.as_ref(),
            )
            .await
        }
        .await;
        if let Err(e) = sent {
            self.discard_if_unsent(&dm.id).await;
            return Err(e);
        }

        Ok(self
            .anon_dm_repo
            .get_conversation(&dm.id)?
            .ok_or(AnonDmError::ConversationNotFound(dm.id))?)
    }

    /// Send a message request using HPKE encryption (NDSS'21 compliant).
    ///
    /// Unlike `send_message_request` which uses Signal Protocol (requiring prekey fetch),
    /// this method encrypts directly to the peer's identity key using HPKE (RFC 9180).
    /// The identity key comes from the private friend code (out-of-band).
    ///
    /// # Security Note
    ///
    /// Phase 1 does NOT have Perfect Forward Secrecy. The init message contains
    /// ONLY key material (mailbox key, identity key, delivery token) - NO sensitive
    /// user data. PFS begins in Phase 2 when Double Ratchet is established.
    pub(crate) async fn send_message_request_ecies(
        &self,
        friend_code: &PrivateFriendCode,
        conversation_id: &AnonDmId,
        our_mailbox_pk: &[u8; 32],
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
        profile_key: Option<&[u8; 32]>,
    ) -> Result<(), AnonDmError> {
        use crate::crypto::ecies;
        use crate::domain::anon_dm_init::AnonDmInitMessage;

        // Get our identity key to include in the initial message
        // This allows recipient to establish a proper Signal session later
        let our_identity_key = self.get_my_identity_key()?;
        let our_delivery_token = profile_key
            .map(PrivateFriendCode::derive_delivery_token)
            .unwrap_or([0u8; 12]);

        // Get our user ID - sender identity is revealed after decryption per NDSS'21 Section II-A
        let our_user_id = self.current_user_id()?;

        // Build the initial contact message per NDSS'21: "init" || pk_e
        // Extended to include our identity for subsequent session establishment
        // NOTE: No avatar in Phase 1 — ECIES has no PFS, so only ephemeral data goes here.
        // Avatars are exchanged after Double Ratchet session is established (Phase 2).
        // Also, server caps ECIES_INIT payloads at 2048 bytes (GRA-572).
        // This device owns our mailbox, so the chat binds the peer to it.
        let our_device_id: u32 = self
            .settings_repo
            .get(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)?
            .ok_or(AnonDmError::NotLoggedIn)?;
        let init_message = AnonDmInitMessage::new(
            *conversation_id,
            our_user_id,
            our_device_id,
            *our_mailbox_pk,
            our_identity_key,
            our_delivery_token,
            sender_display_name.unwrap_or(""),
            intro_message.unwrap_or_default(),
            None, // No avatar in Phase 1 (no PFS, payload size cap)
        );

        // Serialize the init message
        let plaintext = init_message
            .to_bytes()
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // HPKE to recipient's identity key (no prekey fetch!)
        let recipient_identity = libsignal_protocol::IdentityKey::decode(&friend_code.identity_key)
            .map_err(|e| AnonDmError::CryptoError(format!("friend code identity key: {e}")))?;
        let sealed_body = ecies::seal_to_identity(
            &recipient_identity,
            ecies::IdentityHpkeUse::AnonDmInit,
            &plaintext,
        )
        .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Secret-chat traffic goes over Tor or not at all; refuse before the
        // token is spent.
        let mut anon_client = self
            .grpc_pool
            .secret_message_client()
            .map_err(|e| AnonDmError::MessageError(e.to_string()))?;

        // Get blind signature token for anonymous delivery
        let token = self.claim_token()?;

        // Send to recipient's device inbox using anonymous credential
        // The ECIES ciphertext goes in the sealed_body, but we're NOT using
        // sealed sender here - we're using raw ECIES + anonymous credential

        // ECIES_INIT requires server_key_id == 0 (server rejects non-zero, see sealed_sender.go:66-68)
        let sealed_message = SealedSenderMessage {
            sealed_body,
            // Plaintext to the server: coarsened (see now_anonymized).
            timestamp: crate::util::time::TimestampNanos::now_anonymized().as_u64(),
            server_key_id: 0,
        };

        // Use envelope_type=4 (ECIES_INIT) so receiver knows to use HPKE decryption
        const ENVELOPE_TYPE_ECIES_INIT: i32 = 4;

        // From here on the request may leave this device.
        self.anon_dm_repo.mark_request_submitted(conversation_id)?;

        anon_client
            .send_with_anonymous_credential(
                &friend_code.user_id.to_string(),
                vec![crate::proto::chat_v1::SealedDeviceMessage {
                    device_id: 0,
                    registration_id: 0,
                    sealed_message: Some(sealed_message),
                }],
                crate::proto::chat_v1::SealedDelivery::Broadcast,
                ENVELOPE_TYPE_ECIES_INIT,
                &token.public_key,
                &token.signature,
            )
            .await
            .map_err(|e| {
                AnonDmError::MessageError(format!("Failed to send ECIES message: {}", e))
            })?;

        Ok(())
    }

    /// Send a message request to a peer's device inbox.
    pub(crate) async fn send_message_request(
        &self,
        peer_id: &UserId,
        conversation_id: &AnonDmId,
        our_mailbox_pk: &[u8; 32],
        intro_message: Option<&str>,
        profile_key: Option<&[u8; 32]>,
    ) -> Result<(), AnonDmError> {
        // Name under the profile key; the avatar as a pointer to its upload,
        // never inline: this request is sealed to a device inbox (8 KiB cap).
        let (encrypted_display_name, avatar) = if let Some(pk) = profile_key {
            // Encrypt display name
            let enc_name = {
                let display_name = self.get_my_display_name().unwrap_or_default();
                if !display_name.is_empty() {
                    crate::crypto::profile_crypto::encrypt_display_name(pk, &display_name).ok()
                } else {
                    None
                }
            };
            (enc_name, self.my_avatar_pointer().await)
        } else {
            (None, None)
        };

        let request = AnonDmRequest {
            conversation_id: conversation_id.to_vec(),
            initiator_mailbox_pk: our_mailbox_pk.to_vec(),
            intro_message: intro_message.unwrap_or_default().to_string(),
            profile_key: profile_key.map(|k| k.to_vec()),
            timestamp: TimestampNanos::now().as_u64(),
            encrypted_display_name,
            avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmRequest(request)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to peer's device inbox via pairwise sealed sender
        self.send_to_device_inbox(conversation_id, peer_id, content)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::repositories::settings_repo::keys;

    fn client() -> (
        tempfile::TempDir,
        std::sync::Arc<crate::client::ChatterClient>,
    ) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let s = &client.messages().settings_repo;
        s.set(keys::CURRENT_USER_ID, &UserId::new().to_string())
            .unwrap();
        s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        (dir, client)
    }

    /// A start that fails before its request can leave the device (here: no
    /// Tor route for the mailbox) leaves nothing behind, so "Start secret
    /// chat" simply starts again. One that may have left is kept as is.
    #[tokio::test]
    async fn unsent_request_is_discarded_submitted_one_kept() {
        let (_dir, client) = client();
        let svc = client.anon_dms();
        let peer = UserId::new();

        assert!(svc.start_conversation(peer, None).await.is_err());
        assert!(
            svc.anon_dm_repo
                .get_conversation_by_peer(&peer)
                .unwrap()
                .is_none()
        );

        // Submitted (or uncertain): survives a failed restart attempt's cleanup.
        let dm = AnonDm::new_initiated(peer);
        svc.anon_dm_repo.insert_conversation(&dm).unwrap();
        assert!(
            svc.anon_dm_repo
                .get_conversation(&dm.id)
                .unwrap()
                .unwrap()
                .is_unsent_request()
        );
        svc.anon_dm_repo.mark_request_submitted(&dm.id).unwrap();
        svc.discard_if_unsent(&dm.id).await;
        let kept = svc.anon_dm_repo.get_conversation(&dm.id).unwrap().unwrap();
        assert!(kept.request_submitted && !kept.is_unsent_request());
    }

    /// A secret chat is bound to one peer device: the first binding wins, an
    /// acceptance from another device of the peer is refused, and a received
    /// request binds the device it was sealed from.
    #[tokio::test]
    async fn chat_is_bound_to_one_peer_device() {
        let (_dir, client) = client();
        let svc = client.anon_dms();
        let peer = UserId::new();
        let dm = AnonDm::new_initiated(peer);
        svc.anon_dm_repo.insert_conversation(&dm).unwrap();
        svc.anon_dm_repo.pin_peer_device(&dm.id, 3).unwrap();
        svc.anon_dm_repo.pin_peer_device(&dm.id, 3).unwrap();
        assert!(svc.anon_dm_repo.pin_peer_device(&dm.id, 1).is_err());

        let res = svc.handle_acceptance(dm.id, peer, 1, [7; 32], None, None, None);
        assert!(matches!(res, Err(AnonDmError::InvalidData(_))), "{res:?}");
        let still = svc.anon_dm_repo.get_conversation(&dm.id).unwrap().unwrap();
        assert_eq!(still.status, AnonDmStatus::Pending);
        assert_eq!(still.peer_device_id, Some(3));
        svc.handle_acceptance(dm.id, peer, 3, [7; 32], None, None, None)
            .unwrap();
        assert!(
            svc.anon_dm_repo
                .get_conversation(&dm.id)
                .unwrap()
                .unwrap()
                .is_ready()
        );

        let stranger = UserId::new();
        let incoming = AnonDmId::new();
        let got = svc
            .handle_request(incoming, stranger, 5, [9; 32], None, None, None, None)
            .unwrap();
        assert_eq!(got.peer_device_id, Some(5));
        assert_eq!(
            svc.anon_dm_repo
                .get_conversation(&incoming)
                .unwrap()
                .unwrap()
                .peer_device_id,
            Some(5)
        );
    }
}
