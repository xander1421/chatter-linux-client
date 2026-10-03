//! DM bootstrap operations.
//!
//! Handles initial DM conversation setup using the Sesame algorithm
//! from the NDSS '21 paper. The lower UUID always initiates the bootstrap.

use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::mailbox::dm::{
    release_bootstrap_lock, try_acquire_bootstrap_lock, DmBootstrap,
};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
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
        // Check if this is an ephemeral message (typing indicator, receipt) that should be dropped
        if super::dm::is_ephemeral_content(&content) {
            tracing::debug!(
                peer_id = %peer_id,
                "Dropping ephemeral message (typing/receipt) - bootstrap not complete"
            );
            return Err(MessageError::BootstrapInitiated);
        }

        // Get our user ID for Sesame deterministic initiator selection
        let our_user_id = self.settings_repo.current_user_id()?;

        // Sesame algorithm: lower UUID initiates bootstrap, higher UUID waits
        let we_are_initiator = our_user_id.as_uuid() < peer_id.as_uuid();

        if !we_are_initiator {
            return self.handle_dm_bootstrap_responder(
                auth_client,
                peer_id,
                content,
                grpc_pool,
                attachment_ids,
                channel_id,
                &our_user_id,
            ).await;
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

        let dm_bootstrap = DmBootstrap::new(self.database.clone());
        let mut anon_mailbox_client =
            grpc_pool.anonymous_mailbox_client();

        match dm_bootstrap.initiate_dm(*peer_id, &mut anon_mailbox_client).await {
            Ok(bootstrap_result) => {
                self.send_bootstrap_payload(
                    auth_client,
                    peer_id,
                    &devices,
                    &bootstrap_result.bootstrap_message,
                    grpc_pool,
                ).await?;

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
        if conversation.as_ref().map(|c| c.is_ready_to_send()).unwrap_or(false) {
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
        let dm_bootstrap = DmBootstrap::new(self.database.clone());
        let mut anon_mailbox_client = grpc_pool.anonymous_mailbox_client();

        match dm_bootstrap
            .initiate_dm(*peer_id, &mut anon_mailbox_client)
            .await
        {
            Ok(bootstrap_result) => {
                self.send_bootstrap_payload(
                    &mut auth_client,
                    peer_id,
                    &devices,
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
            Err(e) => Err(MessageError::DmError(format!("prewarm bootstrap failed: {e}"))),
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
            if let Err(e) = self.send_bootstrap_request(auth_client, peer_id, our_user_id, grpc_pool).await {
                release_bootstrap_lock(peer_id);
                return Err(e);
            }
        }
        // Held only across the send: a lost REQUEST or MESSAGE must not
        // block the next send's REQUEST — every queued send asks again.
        release_bootstrap_lock(peer_id);

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
        self.send_bootstrap_payload(auth_client, peer_id, &devices, &bootstrap_request(our_user_id), grpc_pool)
            .await
            .map(|_| ())
    }

    /// The devices of `peer_id` a bootstrap MESSAGE or REQUEST goes to,
    /// newest first.
    ///
    /// The Signal session store is our cache of a peer's devices: every
    /// normal-mode DM send's all-devices bundle fetch leaves a session per
    /// current device. Normal mode refreshes it here with that same fetch;
    /// Shield must not name the peer in an identified RPC, so it uses only
    /// what is cached — nothing cached is `NotReachablePrivately`, never a
    /// fetch and never a device-1 guess.
    pub(super) async fn bootstrap_devices(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
        let peer_id_str = peer_id.to_string();
        let shield = self.shield_mode.load(std::sync::atomic::Ordering::Acquire);
        if !shield {
            if let Err(e) = self
                .signal_service
                .ensure_sessions_all_devices(auth_client, &peer_id_str)
                .await
            {
                tracing::warn!(peer_id = %peer_id, error = %e, "Bootstrap: device refresh failed; using cached sessions");
            }
        }
        let devices = bootstrap_device_order(self.live_session_devices(peer_id).await?);
        if devices.is_empty() {
            return Err(if shield {
                MessageError::NotReachablePrivately(*peer_id)
            } else {
                MessageError::DmError(format!("no reachable device of {peer_id} for the DM bootstrap"))
            });
        }
        Ok(devices)
    }

    /// Devices of `peer_id` we hold a session with, less any the server has
    /// refused as retired (`is_retired_device`). No RPC.
    pub(super) async fn live_session_devices(&self, peer_id: &UserId) -> Result<Vec<u32>, MessageError> {
        let mut live = Vec::new();
        for device_id in self
            .signal_service
            .session_devices(&peer_id.to_string())
            .await
            .map_err(|e| MessageError::from_signal_send(e, "Session device lookup"))?
        {
            if !self.is_retired_device(*peer_id, device_id, 0).await {
                live.push(device_id);
            }
        }
        Ok(live)
    }

    /// Seal `payload` (bootstrap MESSAGE or REQUEST) to EVERY device in
    /// `devices`, each under its own session, and deposit each under its own
    /// pre-fetched anonymous token. Returns the devices it reached.
    ///
    /// Every device, not one: each device that answers keeps its own half of
    /// the conversation (`DmConversationManager::record_peer_device`), so an
    /// offline device answering hours later cannot take an online one's
    /// mailbox over, and picking one device for a multi-device peer would
    /// leave the message waiting on whichever was picked. A device the server
    /// refuses as unknown is archived, marked retired and dropped from the
    /// conversation. Succeeds when any device was reached.
    ///
    /// The token comes from the pre-fetched batch only — requesting one here
    /// would tie the authenticated request to this anonymous send in time
    /// (NDSS'21 §VI-F). An empty batch stops the fan-out.
    pub(super) async fn send_bootstrap_payload(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        devices: &[u32],
        payload: &[u8],
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<Vec<u32>, MessageError> {
        let peer_id_str = peer_id.to_string();
        let mut reached = Vec::new();
        let mut last_error = None;

        for &peer_device_id in devices {
            let sealed_ciphertext = match self
                .signal_service
                .encrypt_message(auth_client, &peer_id_str, peer_device_id, payload)
                .await
            {
                Ok(sealed) => sealed,
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "DM bootstrap: encrypt failed for device");
                    last_error = Some(MessageError::EncryptionFailed(format!("Bootstrap encrypt: {}", e)));
                    continue;
                }
            };

            let credential_token = match self.token_manager.claim_prefetched_token() {
                Ok(token) => token,
                Err(e) => {
                    last_error = Some(MessageError::DmError(format!(
                        "No anonymous token for the DM bootstrap ({e}); try again shortly"
                    )));
                    break;
                }
            };

            // Send to peer's device inbox via anonymous client (no AuthInterceptor)
            // to avoid dual-auth rejection — GRA-436
            let sealed_msg = SealedSenderMessage {
                sealed_body: sealed_ciphertext,
                server_key_id: self.signal_service.get_server_key_id(),
                timestamp: TimestampNanos::now().as_u64(),
            };

            match grpc_pool
                .anonymous_message_client()
                .send_with_anonymous_credential(
                    &peer_id_str,
                    peer_device_id,
                    sealed_msg,
                    &credential_token.public_key,
                    &credential_token.signature,
                    credential_token.randomizer.as_deref(),
                )
                .await
            {
                Ok(_) => {
                    tracing::info!(peer_id = %peer_id, peer_device_id, "DM bootstrap payload sent to peer's device inbox");
                    reached.push(peer_device_id);
                    if let Err(e) = crate::mailbox::dm::DmConversationManager::new(self.database.clone())
                        .mark_device_asked(peer_id, peer_device_id)
                    {
                        tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to record the bootstrap ask");
                    }
                }
                Err(e) if e.is_unknown_destination_device() => {
                    tracing::info!(peer_id = %peer_id, peer_device_id, "DM bootstrap: device retired on server, dropping it");
                    self.retire_peer_device(peer_id, peer_device_id).await;
                }
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "DM bootstrap: send failed for device");
                    last_error = Some(MessageError::DmError(format!("Failed to send bootstrap: {}", e)));
                }
            }
        }

        if !reached.is_empty() {
            return Ok(reached);
        }
        Err(last_error.unwrap_or_else(|| {
            if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
                MessageError::NotReachablePrivately(*peer_id)
            } else {
                MessageError::DmError(format!("every known device of {peer_id} is retired"))
            }
        }))
    }

    /// The server refused `peer_device_id` as unknown: mark it retired,
    /// archive its session and drop it from the mailbox conversation.
    async fn retire_peer_device(&self, peer_id: &UserId, peer_device_id: u32) {
        self.retired_peer_devices
            .lock()
            .insert((*peer_id, peer_device_id), std::time::Instant::now());
        if let Err(e) = self
            .signal_service
            .actor
            .archive_session_for_resend(peer_id.to_string(), peer_device_id, None)
            .await
        {
            tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to archive session for retired device");
        }
        if let Err(e) = crate::mailbox::dm::DmConversationManager::new(self.database.clone())
            .forget_peer_device(peer_id, peer_device_id)
        {
            tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to drop retired device from the mailbox conversation");
        }
    }

    /// Ask one peer device that has no half of the conversation yet to make
    /// one: the lower UUID re-announces its live mailbox and key to it (a
    /// bootstrap MESSAGE), the higher sends it a REQUEST. One token.
    pub(super) async fn prompt_peer_device(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        peer_device_id: u32,
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<(), MessageError> {
        let our_user_id = self.settings_repo.current_user_id()?;
        let payload = if our_user_id.as_uuid() < peer_id.as_uuid() {
            DmBootstrap::new(self.database.clone())
                .initiate_dm(*peer_id, &mut grpc_pool.anonymous_mailbox_client())
                .await
                .map_err(|e| MessageError::DmError(format!("re-announce for device {peer_device_id}: {e}")))?
                .bootstrap_message
        } else {
            bootstrap_request(&our_user_id)
        };
        self.send_bootstrap_payload(auth_client, peer_id, &[peer_device_id], &payload, grpc_pool)
            .await
            .map(|_| ())
    }
}

/// A bootstrap REQUEST: a KeyExchange with an empty ephemeral.
fn bootstrap_request(our_user_id: &UserId) -> Vec<u8> {
    crate::proto::content_v1::Content {
        content: Some(crate::proto::content_v1::content::Content::KeyExchange(
            crate::proto::content_v1::KeyExchange {
                ephemeral_pk: vec![], // 0 bytes = bootstrap REQUEST
                conversation_id: our_user_id.to_string(),
                timestamp: TimestampNanos::now().as_u64(),
            },
        )),
        ephemeral_reply_to: None,
        timestamp: TimestampNanos::now().as_u64(),
        padding: vec![],
    }
    .encode_to_vec()
}

/// Newest device first — the server assigns device ids as `MAX + 1`, so the
/// lowest session is the likeliest retired install.
fn bootstrap_device_order(mut devices: Vec<u32>) -> Vec<u32> {
    devices.sort_unstable_by(|a, b| b.cmp(a));
    devices.dedup();
    devices
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use libsignal_protocol::{kem, DeviceId, IdentityKeyPair, KeyPair, PreKeyBundle, ProtocolAddress};
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::bootstrap_device_order;
    use crate::client::ChatterClient;
    use crate::domain::ids::UserId;
    use crate::services::message::{MessageError, MessageService};
    use crate::storage::repositories::settings_repo::keys;

    #[test]
    fn bootstrap_targets_every_device_newest_first() {
        assert_eq!(bootstrap_device_order(vec![1, 6, 3, 6]), vec![6, 3, 1]);
        assert_eq!(bootstrap_device_order(vec![2]), vec![2]);
        assert!(bootstrap_device_order(vec![]).is_empty());
    }

    /// A client with a temp datastore, a local identity, and nothing behind
    /// its server URL: any RPC fails, so an outcome other than a network
    /// error proves none was made.
    fn shield_client(me: UserId) -> (Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let ms = client.messages();
        ms.settings_repo.set_string(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        ms.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        let identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(7));
        ms.signal_service.key_store.set_local_identity(42, &identity.serialize()).unwrap();
        ms.shield_mode.store(true, Ordering::Release);
        (client, dir)
    }

    /// Cache `peer`'s `device` the way a normal-mode send does: a session.
    /// One identity per user, shared by all its devices.
    async fn cache_device(ms: &MessageService, peer: &UserId, device: u32, rng: &mut StdRng) {
        let peer_identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(peer.as_uuid().as_u128() as u64));
        let signed = KeyPair::generate(rng);
        let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);
        let signed_sig = peer_identity.private_key().calculate_signature(&signed.public_key.serialize(), rng).unwrap();
        let kyber_sig = peer_identity.private_key().calculate_signature(&kyber.public_key.serialize(), rng).unwrap();
        let device_id = DeviceId::new(device as u8).unwrap();
        let bundle = PreKeyBundle::new(
            rng.random::<u32>() & 0x3fff,
            device_id,
            None,
            rng.random::<u32>().into(),
            signed.public_key,
            signed_sig.to_vec(),
            rng.random::<u32>().into(),
            kyber.public_key.clone(),
            kyber_sig.to_vec(),
            *peer_identity.identity_key(),
        )
        .unwrap();
        ms.signal_service
            .actor
            .establish_session(ProtocolAddress::new(peer.to_string(), device_id), bundle)
            .await
            .expect("session");
    }

    /// Shield, first contact, nothing cached: a clear refusal — no
    /// identified fetch, no device-1 guess, no mailbox made, nothing queued.
    #[tokio::test]
    async fn shield_bootstrap_without_cached_devices_is_not_reachable_privately() {
        // Lower UUID: we would initiate (make a mailbox, spend a token).
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();

        let sent = ms.send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1)).await;
        assert!(
            matches!(sent, Err(MessageError::NotReachablePrivately(p)) if p == friend),
            "got {sent:?}"
        );
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
        assert!(crate::mailbox::dm::DmConversationManager::new(ms.database.clone())
            .load_conversation(&friend)
            .unwrap()
            .is_none());
    }

    /// Shield uses every cached device, newest first, skipping one the server
    /// has refused as retired — all without a single RPC.
    #[tokio::test]
    async fn shield_bootstrap_targets_cached_devices_newest_first() {
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        let mut rng = StdRng::seed_from_u64(0xF34);
        for device in [1, 6, 3] {
            cache_device(ms, &friend, device, &mut rng).await;
        }

        let mut auth = ms.auth_client();
        assert_eq!(ms.bootstrap_devices(&mut auth, &friend).await.unwrap(), vec![6, 3, 1]);

        ms.retired_peer_devices
            .lock()
            .insert((friend, 6), std::time::Instant::now());
        assert_eq!(ms.bootstrap_devices(&mut auth, &friend).await.unwrap(), vec![3, 1]);
    }

    /// Cache a sender certificate for `me` (device 1) that chains to the dev
    /// trust root this test build pins — its private key is committed in
    /// `deploy/vault/init-and-unseal.sh` — so sealing needs no RPC.
    fn cache_sender_certificate(ms: &MessageService, me: &UserId) {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use libsignal_protocol::{PrivateKey, SenderCertificate, ServerCertificate, Timestamp};
        let rng = &mut StdRng::seed_from_u64(0xCE27);
        let root = PrivateKey::deserialize(&BASE64.decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=").unwrap()).unwrap();
        let server = KeyPair::generate(rng);
        let server_cert = ServerCertificate::new(1, server.public_key, &root, rng).unwrap();
        let identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(7));
        let far_future = Timestamp::from_epoch_millis(u64::MAX / 2);
        let cert = SenderCertificate::new(
            me.to_string(), None, *identity.public_key(), DeviceId::new(1).unwrap(),
            far_future, server_cert, &server.private_key, rng,
        )
        .unwrap();
        ms.settings_repo.set_string(keys::SENDER_CERTIFICATE, &BASE64.encode(cert.serialized().unwrap())).unwrap();
        ms.settings_repo.set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &(u64::MAX / 2)).unwrap();
    }

    /// F39: the responder's post-bootstrap flush sealed every queued message
    /// to device 1 — a guess, and an identified prekey fetch naming the peer
    /// when device 1 had no session. With the peer on device 6 only, the flush
    /// now seals to 6 with no RPC at all (nothing is behind the server URL, so
    /// any fetch would fail the call).
    #[tokio::test]
    async fn shield_flush_seals_to_the_session_device_without_a_prekey_fetch() {
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_sender_certificate(ms, &me);
        cache_device(ms, &friend, 6, &mut StdRng::seed_from_u64(6)).await;

        let mut auth = ms.auth_client();
        let sealed = crate::inbox::streaming::dm_bootstrap_handler::seal_for_peer_mailbox(
            &ms.signal_service, &mut auth, &friend, 6, b"queued", &[7u8; 32],
        )
        .await
        .expect("sealed without any RPC");
        assert_eq!(sealed.len(), 1, "one envelope, for device 6");
    }

    /// A send spends only the pre-fetched batch: with it empty, the send fails
    /// on the empty batch itself — not on a network error from a token
    /// request made in the same call — and makes no mailbox.
    #[tokio::test]
    async fn shield_send_with_empty_token_batch_fails_without_requesting_tokens() {
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(9)).await;

        let sent = ms.send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1)).await;
        match sent {
            Err(MessageError::DmError(msg)) => assert!(msg.contains("no tokens available"), "got {msg}"),
            other => panic!("expected the empty-batch refusal, got {other:?}"),
        }
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 0);
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
    }

    /// Our side of a conversation whose peer mailbox is gone (expired, or
    /// dropped as by v069): a live mailbox of ours and an established key,
    /// nothing to send to. Returns our mailbox public key.
    fn seed_half_open(ms: &MessageService, peer: &UserId, key: &crate::mailbox::dm::ConversationKey, pk: u8) -> [u8; 32] {
        let mailbox = crate::mailbox::dm::DmMailbox {
            id: format!("mb-{pk}"),
            public_key: [pk; 32],
            private_key: [pk; 64],
            encryption_private_key: [pk; 32],
            signature: vec![],
            randomizer: None,
            token_id: format!("tok-{pk}"),
            created_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(40),
            last_polled_at: None,
        };
        // The mailbox's token, already spent: the batch stays empty.
        let conn = ms.database.connection().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at) VALUES ('k', x'00', 0, '')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailbox_tokens (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at, used_at)
             VALUES (?1, ?2, x'00', x'00', 'k', '', 'spent')",
            rusqlite::params![mailbox.token_id, mailbox.public_key.as_slice()],
        )
        .unwrap();
        drop(conn);
        let conversations = crate::mailbox::dm::DmConversationManager::new(ms.database.clone());
        conversations.save_our_mailbox(peer, &mailbox).unwrap();
        conversations.save_conversation_key(peer, key).unwrap();
        mailbox.public_key
    }

    /// F51: with neither side holding the other's mailbox, the higher-UUID
    /// side must ask (send a REQUEST) rather than queue and wait for a peer
    /// that has nothing to send. Here the REQUEST gets as far as the empty
    /// token batch — before, the send queued silently and returned
    /// `BootstrapInitiated`.
    #[tokio::test]
    async fn higher_uuid_without_a_peer_mailbox_sends_a_request() {
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(11)).await;
        cache_sender_certificate(ms, &me);
        let (mut ours, mut theirs) = (crate::mailbox::dm::ConversationKey::new(), crate::mailbox::dm::ConversationKey::new());
        ours.derive_shared_secret(theirs.our_ephemeral_public).unwrap();
        theirs.derive_shared_secret(ours.our_ephemeral_public).unwrap();
        seed_half_open(ms, &friend, &ours, 5);

        let sent = ms.send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1)).await;
        match sent {
            Err(MessageError::DmError(msg)) => assert!(msg.contains("No anonymous token for the DM bootstrap"), "got {msg}"),
            other => panic!("expected the REQUEST to be attempted, got {other:?}"),
        }
    }

    /// F51 recovery: the lower-UUID side re-announces its live mailbox and
    /// key (no token, no new mailbox), and the higher side — established on
    /// that same key but missing the peer mailbox — takes it: it records the
    /// mailbox for the announcing device and can send again. F53: a later
    /// re-bootstrap with a new ephemeral (the lower side's mailbox expired)
    /// replaces the working conversation's keys rather than being refused.
    #[tokio::test]
    async fn half_open_conversation_recovers_by_re_announce() {
        use crate::mailbox::dm::{ConversationKey, DmBootstrap, DmConversationManager};
        use crate::error::MailboxError;
        use prost::Message as _;

        let lower = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let higher = UserId::from_uuid(uuid::Uuid::max());
        let (lower_client, _d1) = shield_client(lower);
        let (higher_client, _d2) = shield_client(higher);
        let (lo, hi) = (lower_client.messages(), higher_client.messages());

        let (mut lo_key, mut hi_key) = (ConversationKey::new(), ConversationKey::new());
        lo_key.derive_shared_secret(hi_key.our_ephemeral_public).unwrap();
        hi_key.derive_shared_secret(lo_key.our_ephemeral_public).unwrap();
        let lo_mailbox = seed_half_open(lo, &higher, &lo_key, 5);
        let hi_mailbox = seed_half_open(hi, &lower, &hi_key, 6);

        // Lower side: re-announce, with an empty token batch (a fresh mailbox would fail).
        let announced = DmBootstrap::new(lo.database.clone())
            .initiate_dm(higher, &mut lo.grpc_pool.anonymous_mailbox_client())
            .await
            .expect("re-announce needs no token and no RPC");
        assert_eq!(announced.our_mailbox.public_key, lo_mailbox);
        assert_eq!(announced.our_ephemeral_public, lo_key.our_ephemeral_public);
        let content = crate::proto::content_v1::Content::decode(announced.bootstrap_message.as_slice()).unwrap();
        let Some(crate::proto::content_v1::content::Content::KeyExchange(kx)) = content.content else {
            panic!("bootstrap MESSAGE is a KeyExchange");
        };

        // Higher side takes it from the lower side's device 3.
        let bootstrap = DmBootstrap::new(hi.database.clone());
        let response = bootstrap
            .handle_incoming_bootstrap(lower, 3, &kx.ephemeral_pk, &mut hi.grpc_pool.anonymous_mailbox_client())
            .await
            .expect("accepted")
            .expect("answered with our mailbox");
        assert_eq!(response.our_mailbox_public_key, hi_mailbox);
        let conv = DmConversationManager::new(hi.database.clone()).load_conversation(&lower).unwrap().unwrap();
        assert!(conv.is_ready_to_send());
        let peer_mailbox = &conv.peer_mailboxes[0];
        assert_eq!((peer_mailbox.public_key, peer_mailbox.device_id), (lo_mailbox, 3));
        assert_eq!(conv.device_key(peer_mailbox).unwrap().send_key(), lo_key.recv_key());

        // F53: the lower side's mailbox expires, so it re-bootstraps with a
        // fresh mailbox and ephemeral. The higher side still holds a working
        // conversation, and takes the new keys anyway: the sender is the
        // authenticated peer (the Signal layer refuses any other identity).
        let mut new_lo_key = ConversationKey::new();
        let new_lo_mailbox = [0x5Au8; 32];
        let (_, new_lo_encryption_pk) = crate::crypto::envelope_crypto::generate_mailbox_encryption_key();
        let rebootstrap = crate::mailbox::dm::bootstrap::MailboxAnnouncement {
            mailbox_pk: new_lo_mailbox,
            conversation_pk: new_lo_key.our_ephemeral_public,
            mailbox_encryption_pk: new_lo_encryption_pk,
        }
        .encode();
        let response = bootstrap
            .handle_incoming_bootstrap(lower, 3, &rebootstrap, &mut hi.grpc_pool.anonymous_mailbox_client())
            .await
            .expect("re-bootstrap accepted")
            .expect("answered with our mailbox");
        assert_eq!(response.peer_mailbox_public_key, new_lo_mailbox);
        assert_eq!(response.peer_mailbox_encryption_key, new_lo_encryption_pk);
        new_lo_key.derive_shared_secret(response.our_ephemeral_public).unwrap();
        let conv = DmConversationManager::new(hi.database.clone()).load_conversation(&lower).unwrap().unwrap();
        assert!(conv.is_ready_to_send());
        assert_eq!(conv.peer_mailboxes.len(), 1, "device 3's half replaced, not added");
        assert_eq!(conv.peer_mailboxes[0].public_key, new_lo_mailbox);
        let hi_key = conv.device_key(&conv.peer_mailboxes[0]).unwrap();
        assert_eq!(hi_key.send_key(), new_lo_key.recv_key());
        assert_ne!(hi_key.send_key(), lo_key.recv_key(), "the old secret is gone");

        // A bootstrap from any other identity is filed under that identity:
        // it cannot touch the conversation with `lower`.
        let mallory = UserId::from_uuid(uuid::Uuid::from_u128(2));
        let hostile = crate::mailbox::dm::bootstrap::MailboxAnnouncement {
            mailbox_pk: [0x66u8; 32],
            conversation_pk: ConversationKey::new().our_ephemeral_public,
            mailbox_encryption_pk: [0x67u8; 32],
        }
        .encode();
        match bootstrap.handle_incoming_bootstrap(mallory, 3, &hostile, &mut hi.grpc_pool.anonymous_mailbox_client()).await {
            // No conversation with mallory: needs a fresh mailbox, and the token batch is empty.
            Err(MailboxError::NoTokensAvailable) => {}
            other => panic!("expected the empty token batch, got {:?}", other.map(|r| r.is_some())),
        }
        let conv = DmConversationManager::new(hi.database.clone()).load_conversation(&lower).unwrap().unwrap();
        assert_eq!(conv.peer_mailboxes[0].public_key, new_lo_mailbox);
        assert_eq!(conv.device_key(&conv.peer_mailboxes[0]).unwrap().send_key(), new_lo_key.recv_key());
    }

    /// `n` claimable pre-fetched tokens.
    fn seed_tokens(ms: &MessageService, n: usize) {
        let conn = ms.database.connection().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at) VALUES ('live', x'00', ?1, '')",
            [i64::MAX],
        )
        .unwrap();
        for i in 0..n {
            conn.execute(
                "INSERT INTO mailbox_tokens (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at)
                 VALUES (?1, ?2, ?3, x'00', 'live', ?4)",
                rusqlite::params![format!("t{i}"), [0xA0 + i as u8; 32].as_slice(), [0xA0 + i as u8; 64].as_slice(), format!("{i:04}")],
            )
            .unwrap();
        }
    }

    /// F54: a REQUEST goes to EVERY live device of a multi-device peer, each
    /// sealed to that device under its own token — not to the newest one
    /// only (the offline Tab while the S9 was online). Nothing is behind the
    /// server URL, so each deposit fails after its token is spent: four
    /// devices, four tokens.
    #[tokio::test]
    async fn request_fans_out_to_every_live_device() {
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_sender_certificate(ms, &me);
        let mut rng = StdRng::seed_from_u64(0xF54);
        for device in [2, 6, 7, 8] {
            cache_device(ms, &friend, device, &mut rng).await;
        }
        seed_half_open(ms, &friend, &crate::mailbox::dm::ConversationKey::new(), 5);
        seed_tokens(ms, 10);

        let sent = ms.send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1)).await;
        assert!(matches!(sent, Err(MessageError::DmError(ref m)) if m.contains("Failed to send bootstrap")), "got {sent:?}");
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 6, "one REQUEST per device");
    }

    /// F54: with device 6 ready, a send owes its copy to devices 7 and 8
    /// (no half yet) and asks each of them. An ask that did not get through
    /// (nothing is behind the server URL) is repeated on the next send; one
    /// that did is not. Device 6, which has the message, is owed nothing.
    #[tokio::test]
    async fn unready_devices_are_owed_each_copy_and_asked_once() {
        use crate::mailbox::dm::{ConversationKey, DmConversationManager};
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_sender_certificate(ms, &me);
        let mut rng = StdRng::seed_from_u64(0xF55);
        for device in [6, 7, 8] {
            cache_device(ms, &friend, device, &mut rng).await;
        }
        seed_half_open(ms, &friend, &ConversationKey::new(), 5);
        let conversations = DmConversationManager::new(ms.database.clone());
        conversations
            .record_peer_device(&friend, 6, [6u8; 32], ConversationKey::new().our_ephemeral_public)
            .unwrap();
        let conv = conversations.load_conversation(&friend).unwrap().unwrap();
        assert!(conv.is_ready_to_send());
        seed_tokens(ms, 10);

        let mut auth = ms.auth_client();
        ms.owe_unready_devices(&mut auth, &friend, &conv, b"one", Some(&ms.grpc_pool)).await;
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 8, "7 and 8 asked");
        ms.owe_unready_devices(&mut auth, &friend, &conv, b"two", Some(&ms.grpc_pool)).await;
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 6, "failed asks repeated");
        for device in [7, 8] {
            conversations.mark_device_asked(&friend, device).unwrap();
        }
        ms.owe_unready_devices(&mut auth, &friend, &conv, b"three", Some(&ms.grpc_pool)).await;
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 6, "a delivered ask is not repeated");

        assert!(conversations.drain_device_queue(&friend, 6).unwrap().is_empty());
        for device in [7, 8] {
            assert_eq!(
                conversations.drain_device_queue(&friend, device).unwrap(),
                vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
            );
        }
    }

    /// F55: a device that answers after another already took the Sesame queue
    /// finds that queue empty; what it is owed (queued when the first device
    /// got the copies) still goes to it. The responder flush used to return on
    /// the empty queue, so the late Tab never got rc4–rc6. Nothing is behind
    /// the server URL: the deposit fails, but the owed copy was taken for it.
    #[tokio::test]
    async fn late_device_gets_what_it_is_owed_when_nothing_is_queued() {
        use crate::mailbox::dm::{ConversationKey, DmConversationManager};
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_sender_certificate(ms, &me);
        let mut rng = StdRng::seed_from_u64(0xF55B);
        for device in [6, 8] {
            cache_device(ms, &friend, device, &mut rng).await;
        }
        seed_half_open(ms, &friend, &ConversationKey::new(), 5);
        let conversations = DmConversationManager::new(ms.database.clone());
        conversations.queue_for_device(&friend, 8, b"rc4").unwrap();
        conversations
            .record_peer_device(&friend, 8, [8u8; 32], ConversationKey::new().our_ephemeral_public)
            .unwrap();
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());

        let mut auth = ms.auth_client();
        let flush = crate::inbox::streaming::dm_bootstrap_handler::send_pending_messages_after_bootstrap(
            friend, 8, ms.database.clone(), &ms.signal_service, &mut auth, ms.grpc_pool.anon_channel(), ms,
        )
        .await;
        assert!(flush.is_none(), "nothing queued, nothing to announce");
        assert!(conversations.drain_device_queue(&friend, 8).unwrap().is_empty(), "owed copy taken for device 8");
    }
}
