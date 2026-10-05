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
                    peer_id = %peer_id,
                    old_conversation_id = %existing.id,
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
        .await?;

        tracing::info!(
            "Started anonymous DM conversation {} with {}",
            dm.id,
            peer_id
        );

        Ok(dm)
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
                    peer_id = %peer_id,
                    old_conversation_id = %existing.id,
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
        .await?;

        tracing::info!(
            conversation_id = %dm.id,
            peer_id = %peer_id,
            "Started NDSS'21-compliant anonymous DM conversation (no prekey fetch)"
        );

        Ok(dm)
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
        let init_message = AnonDmInitMessage::new(
            *conversation_id,
            our_user_id,
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

        // Get blind signature token for anonymous delivery
        let token = self.claim_token()?;

        // Send to recipient's device inbox using anonymous credential
        // The ECIES ciphertext goes in the sealed_body, but we're NOT using
        // sealed sender here - we're using raw ECIES + anonymous credential
        let mut anon_client = self.grpc_pool.anonymous_message_client();

        // ECIES_INIT requires server_key_id == 0 (server rejects non-zero, see sealed_sender.go:66-68)
        let sealed_message = SealedSenderMessage {
            sealed_body,
            // Plaintext to the server: coarsened (see now_anonymized).
            timestamp: crate::util::time::TimestampNanos::now_anonymized().as_u64(),
            server_key_id: 0,
        };

        // Use envelope_type=4 (ECIES_INIT) so receiver knows to use HPKE decryption
        const ENVELOPE_TYPE_ECIES_INIT: i32 = 4;

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
                token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| {
                AnonDmError::MessageError(format!("Failed to send ECIES message: {}", e))
            })?;

        tracing::debug!(
            conversation_id = %conversation_id,
            peer_id = %friend_code.user_id,
            "Sent ECIES-encrypted message request (NDSS'21 compliant)"
        );

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
        // Encrypt display name and avatar with profile key if available
        let (encrypted_display_name, encrypted_avatar) = if let Some(pk) = profile_key {
            // Encrypt display name
            let enc_name = {
                let display_name = self.get_my_display_name().unwrap_or_default();
                if !display_name.is_empty() {
                    crate::crypto::profile_crypto::encrypt_display_name(pk, &display_name).ok()
                } else {
                    None
                }
            };
            // Encrypt avatar (stored locally, shared encrypted during bootstrap)
            let enc_avatar = self.get_encrypted_avatar(pk);
            (enc_name, enc_avatar)
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
            encrypted_avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmRequest(request)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to peer's device inbox via pairwise sealed sender
        self.send_to_device_inbox(peer_id, content).await?;

        tracing::debug!(
            "Sent message request for conversation {} to {}",
            conversation_id,
            peer_id
        );
        Ok(())
    }
}
