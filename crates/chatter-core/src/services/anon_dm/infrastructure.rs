use crate::domain::UserId;
use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmMailbox, RequestState};
use crate::proto::content_v1::Content;
use crate::proto::mailbox_v1::EnvelopeType;
use chrono::Utc;
use prost::Message as ProstMessage;
use uuid::Uuid;

use super::{AnonDmError, AnonDmService};

/// The send RPC's idempotency key for our request in `conversation_id`: the
/// same on every resend, so the server queues the request at most once per
/// device. Hashed so the server never sees the conversation id, and shaped
/// like any random v4 UUID.
pub(crate) fn request_id(conversation_id: &AnonDmId) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(b"chatter.secret-chat.request-id")
        .chain_update(conversation_id.to_string())
        .finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

impl AnonDmService {
    /// Send our message request for `conversation_id` to ONE of the peer's
    /// device inboxes, fully anonymously over Tor
    /// ([`PairwiseService::send_to_device_inbox`]). A first attempt lets the
    /// server pick the peer's current device and binds the chat to it; a
    /// resend goes to that device. The outcome is recorded by
    /// [`Self::settle_request_send`].
    ///
    /// [`PairwiseService::send_to_device_inbox`]: crate::services::pairwise::PairwiseService::send_to_device_inbox
    pub(crate) async fn send_to_device_inbox(
        &self,
        conversation_id: &AnonDmId,
        peer_id: &UserId,
        content: Content,
    ) -> Result<(), AnonDmError> {
        use crate::services::pairwise::PairwiseError;
        let repo = &self.anon_dm_repo;
        let dm = repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;
        let mut reached_rpc = false;
        let result = self
            .pairwise_service
            .send_to_device_inbox(
                &self.token_manager,
                &request_id(conversation_id),
                peer_id,
                dm.peer_device_id,
                &content.encode_to_vec(),
                |device| {
                    repo.pin_peer_device(conversation_id, device)?;
                    repo.set_request_state(conversation_id, RequestState::Uncertain)?;
                    reached_rpc = true;
                    Ok(())
                },
            )
            .await;
        if reached_rpc {
            let rpc = match &result {
                Ok(_) => Ok(()),
                Err(PairwiseError::Network(e)) => Err(e),
                // Only the RPC fails after the hook.
                Err(_) => Err(&crate::error::NetworkError::Unavailable),
            };
            self.settle_request_send(&dm, rpc)?;
        }
        result?;
        Ok(())
    }

    /// Record how a request's send RPC ended. Marked [`RequestState::Uncertain`]
    /// right before the RPC; the server accepting it makes it
    /// [`RequestState::Submitted`]. A definite refusal of the FIRST attempt
    /// (`dm` was [`RequestState::Unsent`]) proves nothing left this device, so
    /// it goes back to `Unsent` and the caller discards it. Anything else
    /// stays uncertain — a resend may be refused after an earlier attempt
    /// went through — and is resent under the same [`request_id`].
    pub(crate) fn settle_request_send(
        &self,
        dm: &AnonDm,
        rpc: Result<(), &crate::error::NetworkError>,
    ) -> Result<(), AnonDmError> {
        let state = match rpc {
            Ok(()) => RequestState::Submitted,
            Err(e) if dm.request == RequestState::Unsent && e.is_definite_rejection() => {
                RequestState::Unsent
            }
            Err(_) => return Ok(()),
        };
        self.anon_dm_repo.set_request_state(&dm.id, state)?;
        Ok(())
    }

    /// Delete one of our mailboxes on the server, proving we hold its key
    /// (challenge-response over the secret route, like a poll).
    pub(crate) async fn delete_server_mailbox(
        &self,
        mailbox: &crate::domain::anon_dm::AnonDmMailbox,
    ) -> Result<(), AnonDmError> {
        use ed25519_dalek::{Signer, SigningKey};

        let net = |e: crate::error::NetworkError| AnonDmError::MessageError(e.to_string());
        let mut client = self.grpc_pool.secret_mailbox_client().map_err(net)?;
        let challenge = client
            .get_mailbox_challenge(mailbox.public_key.to_vec())
            .await
            .map_err(net)?
            .challenge;
        let signature = SigningKey::from_keypair_bytes(&mailbox.private_key)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
            .sign(&challenge);
        client
            .delete_ephemeral_mailbox(
                mailbox.public_key.to_vec(),
                signature.to_bytes().to_vec(),
                challenge,
            )
            .await
            .map_err(net)
    }

    /// Claim a token from the pre-fetched batch (`claim_prefetched_token`).
    ///
    /// Never requests tokens: an empty batch fails the send rather than
    /// linking an authenticated token request to this anonymous spend
    /// (NDSS'21 §VI-F).
    pub(crate) fn claim_token(
        &self,
    ) -> Result<crate::mailbox::token_manager::MailboxToken, AnonDmError> {
        self.token_manager
            .claim_prefetched_token()
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))
    }

    /// Send content to a peer's anonymous mailbox.
    pub(crate) async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonDmError> {
        // Secret-chat traffic goes over Tor or not at all; refuse before any
        // submission is recorded or token spent.
        self.grpc_pool
            .secret_channel()
            .map_err(|e| AnonDmError::MessageError(e.to_string()))?;
        // Serialize content
        let plaintext = content.encode_to_vec();
        let retained = if crate::services::gateway::submission::user_post(&content) {
            let owner = self.current_user_id()?;
            match crate::services::gateway::submission::prepare(
                self.settings_repo.database(),
                &owner.to_string(),
                &peer_id.to_string(),
                &plaintext,
            )? {
                crate::services::gateway::submission::Admission::First(key) => Some(key),
                crate::services::gateway::submission::Admission::Confirmed => return Ok(()),
                crate::services::gateway::submission::Admission::Unconfirmed => {
                    crate::services::gateway::submission::mark_existing_submission();
                    return Err(AnonDmError::DeliveryUnconfirmed);
                }
            }
        } else {
            None
        };

        // Encrypt to the ONE peer device this chat is bound to, never over the
        // normal transport. With a session: local encrypt. Without one (first
        // contact as responder, or after the session was archived):
        // re-establish anonymously (blind token + anonymous fetch over Tor) —
        // an authenticated prekey fetch would leak this chat's
        // sender->recipient edge. A chat with no bound device sends nothing.
        let discard = |e: AnonDmError| {
            if let Some(key) = retained.as_ref() {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            e
        };
        let device = self
            .anon_dm_repo
            .get_conversation_by_peer(peer_id)
            .map_err(|e| discard(e.into()))?
            .and_then(|dm| dm.peer_device_id)
            .ok_or_else(|| {
                discard(AnonDmError::InvalidData(
                    "secret chat is not bound to a peer device".into(),
                ))
            })?;
        let has_session = self
            .pairwise_service
            .has_session(peer_id, device)
            .await
            .map_err(|e| discard(e.into()))?;
        // Our certificate first (over Tor if it must be fetched), so a missing
        // one fails before any token is spent or submission started.
        self.pairwise_service
            .ensure_sender_certificate()
            .await
            .map_err(|e| discard(e.into()))?;
        let ciphertext = if has_session {
            crate::services::gateway::submission::mark_submission_started()
                .map_err(|e| discard(e.into()))?;
            self.pairwise_service
                .encrypt_existing_session(peer_id, device, &plaintext)
                .await?
        } else {
            let token = self.claim_token().map_err(discard)?;
            crate::services::gateway::submission::mark_submission_started()
                .map_err(|e| discard(e.into()))?;
            self.pairwise_service
                .encrypt_anonymous(peer_id, Some(device), &plaintext, &token)
                .await?
                .0
        };

        if let Some(key) = retained.as_ref() {
            crate::services::gateway::submission::ciphertext(
                self.settings_repo.database(),
                key,
                &ciphertext,
            )
            .map_err(|_| AnonDmError::DeliveryUnconfirmed)?;
        }
        // Send to anonymous mailbox
        self.mailbox_manager
            .send_to_secret_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "[ANON_DM_MAILBOX] Failed to send to mailbox");
                if retained.is_some() {
                    AnonDmError::DeliveryUnconfirmed
                } else {
                    AnonDmError::MessageError(e.to_string())
                }
            })?;
        if let Some(key) = retained.as_ref() {
            crate::services::gateway::submission::confirm(self.settings_repo.database(), key)
                .map_err(|_| AnonDmError::DeliveryUnconfirmed)?;
        }

        Ok(())
    }

    /// Create a mailbox for a conversation using blind signature tokens.
    pub(crate) async fn create_conversation_mailbox(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<AnonDmMailbox, AnonDmError> {
        // Over Tor or not at all; refuse before the token is spent.
        self.grpc_pool
            .secret_channel()
            .map_err(|e| AnonDmError::MessageError(e.to_string()))?;
        // Atomically claim a blind-signed token (marks used in same SQL stmt)
        let token = self.claim_token()?;

        // Use the token's Ed25519 keypair for the mailbox
        let public_key: [u8; 32] = token
            .public_key
            .clone()
            .try_into()
            .map_err(|_| AnonDmError::InvalidData("Invalid public key length".to_string()))?;
        let private_key: [u8; 64] = token
            .private_key
            .clone()
            .try_into()
            .map_err(|_| AnonDmError::InvalidData("Invalid private key length".to_string()))?;

        // Register the mailbox with the server using the blind signature (ANONYMOUS - no auth headers)
        // Retry on transient transport errors (connection not ready, HTTP/2 reset, etc.)
        let ttl_seconds = 30 * 24 * 60 * 60; // 30 days
        let mut last_err = None;
        let mut response_result = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)))
                    .await;
                tracing::info!(attempt = attempt + 1, "Retrying create_ephemeral_mailbox");
            }
            let mut mailbox_client = self
                .grpc_pool
                .secret_mailbox_client()
                .map_err(|e| AnonDmError::MessageError(e.to_string()))?;
            match mailbox_client
                .create_ephemeral_mailbox(
                    token.public_key.clone(),
                    token.signature.clone(),
                    ttl_seconds,
                )
                .await
            {
                Ok(resp) => {
                    response_result = Some(resp);
                    break;
                }
                Err(e) if e.is_retryable() => {
                    tracing::warn!(attempt = attempt + 1, error = %e, "create_ephemeral_mailbox transient error, disconnecting stale channel and retrying");
                    self.grpc_pool.disconnect_secret();
                    last_err = Some(e);
                }
                Err(e) => {
                    return Err(AnonDmError::MessageError(format!(
                        "Failed to create mailbox on server: {}",
                        e
                    )));
                }
            }
        }
        let response = response_result.ok_or_else(|| {
            AnonDmError::MessageError(format!(
                "Failed to create mailbox on server after 3 attempts: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            ))
        })?;

        tracing::info!(
            conversation_id = %conversation_id,
            mailbox_id = %response.mailbox_id,
            expires_at = response.expires_at,
            "Created ephemeral mailbox for anonymous DM"
        );

        let now = Utc::now();
        let mailbox = AnonDmMailbox {
            id: Uuid::new_v4().to_string(),
            conversation_id: *conversation_id,
            public_key,
            private_key,
            token_id: token.id.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::days(30),
        };

        Ok(mailbox)
    }
}
