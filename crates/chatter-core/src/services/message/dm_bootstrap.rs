//! DM bootstrap operations.
//!
//! Handles initial DM conversation setup using the Sesame algorithm
//! from the NDSS '21 paper. The lower UUID always initiates the bootstrap.

use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::mailbox::dm::{DmBootstrap, release_bootstrap_lock, try_acquire_bootstrap_lock};
use crate::network::grpc::AnonymousMailboxClient;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::{SealedDelivery, SealedDeviceMessage, SealedSenderMessage};
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
    fn bootstrap_mailbox_client(
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

    /// The devices of `peer_id` a bootstrap MESSAGE or REQUEST goes to,
    /// newest first.
    ///
    /// The Signal session store is our cache of a peer's devices: every
    /// normal-mode DM send leaves a session per current device (the server
    /// names one we are missing). Normal mode fetches bundles here only when
    /// it holds no session at all, and its bootstrap send is checked against
    /// the peer's registered devices ([`Self::bootstrap_delivery`]); Shield
    /// must not name the peer in an identified RPC, so it uses only what is
    /// cached — nothing cached is `NotReachablePrivately`, never a fetch and
    /// never a device-1 guess.
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
                MessageError::DmError(format!(
                    "no reachable device of {peer_id} for the DM bootstrap"
                ))
            });
        }
        Ok(devices)
    }

    /// Devices of `peer_id` we hold a session with, less any the server has
    /// refused as retired (`is_retired_device`). No RPC.
    pub(super) async fn live_session_devices(
        &self,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
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

    /// How a bootstrap to the devices [`Self::bootstrap_devices`] returned is
    /// sent: in normal mode those are every device we hold a live session
    /// with, so the server checks them against the peer's registered devices
    /// (`AllDevices`) and names one linked since; Shield must not refresh
    /// sessions with identified prekey fetches, so it sends to the devices it
    /// has (`NamedDevices`).
    pub(super) fn bootstrap_delivery(&self) -> SealedDelivery {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            SealedDelivery::NamedDevices
        } else {
            SealedDelivery::AllDevices
        }
    }

    /// Seal `payload` (bootstrap MESSAGE or REQUEST) to EVERY device in
    /// `devices`, each under its own session, and deposit them all in ONE
    /// `SendSealedMessage` under one pre-fetched anonymous token: queued for
    /// every device or none. Returns the devices it reached.
    ///
    /// Every device, not one: each device that answers keeps its own half of
    /// the conversation (`DmConversationManager::record_peer_device`), so an
    /// offline device answering hours later cannot take an online one's
    /// mailbox over, and picking one device for a multi-device peer would
    /// leave the message waiting on whichever was picked.
    ///
    /// The server's answer is handled once, then the send is retried with a
    /// fresh token: a device it reports unregistered (`extra`) is archived,
    /// marked retired and dropped from the conversation; with `AllDevices`,
    /// missing and stale devices get fresh sessions (`reconcile_devices`)
    /// and the payload is sealed again for the live set.
    ///
    /// The token comes from the pre-fetched batch only — requesting one here
    /// would tie the authenticated request to this anonymous send in time
    /// (NDSS'21 §VI-F). An empty batch stops the send.
    pub(super) async fn send_bootstrap_payload(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        devices: &[u32],
        delivery: SealedDelivery,
        payload: &[u8],
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<Vec<u32>, MessageError> {
        let peer_id_str = peer_id.to_string();
        let mut devices = devices.to_vec();
        let mut last_error = None;

        for attempt in 0..2 {
            let mut messages = Vec::with_capacity(devices.len());
            for &peer_device_id in &devices {
                crate::services::gateway::submission::mark_submission_started()?;
                match self
                    .signal_service
                    .encrypt_message(auth_client, &peer_id_str, peer_device_id, payload)
                    .await
                {
                    Ok(sealed_body) => messages.push(SealedDeviceMessage {
                        device_id: peer_device_id,
                        registration_id: self
                            .signal_service
                            .session_registration_id(&peer_id_str, peer_device_id)
                            .unwrap_or(0),
                        sealed_message: Some(SealedSenderMessage {
                            sealed_body,
                            server_key_id: self.signal_service.get_server_key_id(),
                            timestamp: TimestampNanos::now_anonymized().as_u64(),
                        }),
                    }),
                    Err(e) => {
                        tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "DM bootstrap: encrypt failed for device");
                        last_error = Some(MessageError::EncryptionFailed(format!(
                            "Bootstrap encrypt: {}",
                            e
                        )));
                    }
                }
            }
            if messages.is_empty() && delivery == SealedDelivery::NamedDevices {
                break;
            }

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
            let mut message_client = if self.shield_mode.load(std::sync::atomic::Ordering::Acquire)
            {
                grpc_pool
                    .secret_message_client()
                    .map_err(MessageError::Network)?
            } else {
                grpc_pool.anonymous_message_client()
            };
            match message_client
                .send_with_anonymous_credential(
                    &uuid::Uuid::new_v4().to_string(),
                    &peer_id_str,
                    messages,
                    delivery,
                    0,
                    &credential_token.public_key,
                    &credential_token.signature,
                )
                .await
            {
                Ok(response) => {
                    let manager =
                        crate::mailbox::dm::DmConversationManager::new(self.database.clone());
                    for &peer_device_id in &response.delivered_to_devices {
                        tracing::info!(peer_id = %peer_id, peer_device_id, "DM bootstrap payload sent to peer's device inbox");
                        if let Err(e) = manager.mark_device_asked(peer_id, peer_device_id) {
                            tracing::warn!(peer_id = %peer_id, peer_device_id, error = %e, "Failed to record the bootstrap ask");
                        }
                    }
                    if !response.delivered_to_devices.is_empty() {
                        return Ok(response.delivered_to_devices);
                    }
                    break;
                }
                Err(crate::error::NetworkError::MismatchedDevices {
                    missing,
                    extra,
                    stale,
                }) => {
                    tracing::info!(peer_id = %peer_id, ?missing, ?extra, ?stale, "DM bootstrap: peer's devices changed");
                    for &peer_device_id in &extra {
                        self.retire_peer_device(peer_id, peer_device_id).await;
                    }
                    last_error = Some(MessageError::Network(
                        crate::error::NetworkError::MismatchedDevices {
                            missing: missing.clone(),
                            extra: extra.clone(),
                            stale: stale.clone(),
                        },
                    ));
                    if attempt > 0 {
                        break;
                    }
                    if delivery == SealedDelivery::AllDevices {
                        // Registered after all: no longer retired here.
                        {
                            let mut retired = self.retired_peer_devices.lock();
                            for &d in missing.iter().chain(&stale) {
                                retired.remove(&(*peer_id, d));
                            }
                        }
                        if let Err(e) = self
                            .signal_service
                            .reconcile_devices(auth_client, &peer_id_str, &missing, &[], &stale)
                            .await
                        {
                            last_error = Some(MessageError::from_signal_send(
                                e,
                                "DM bootstrap device reconciliation",
                            ));
                            break;
                        }
                        devices = self.live_session_devices(peer_id).await?;
                    } else {
                        devices.retain(|d| !extra.contains(d));
                    }
                }
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, error = %e, "DM bootstrap: send failed");
                    last_error = Some(MessageError::DmError(format!(
                        "Failed to send bootstrap: {}",
                        e
                    )));
                    break;
                }
            }
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
            let mut mailbox_client = self.bootstrap_mailbox_client(grpc_pool)?;
            DmBootstrap::new(self.database.clone())
                .initiate_dm(*peer_id, &mut mailbox_client)
                .await
                .map_err(|e| {
                    MessageError::DmError(format!("re-announce for device {peer_device_id}: {e}"))
                })?
                .bootstrap_message
        } else {
            bootstrap_request(&our_user_id)
        };
        self.send_bootstrap_payload(
            auth_client,
            peer_id,
            &[peer_device_id],
            SealedDelivery::NamedDevices,
            &payload,
            grpc_pool,
        )
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
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use libsignal_protocol::{
        DeviceId, IdentityKeyPair, KeyPair, PreKeyBundle, ProtocolAddress, kem,
    };
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
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let ms = client.messages();
        ms.settings_repo
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        ms.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();
        let identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(7));
        ms.signal_service
            .key_store
            .set_local_identity(42, &identity.serialize())
            .unwrap();
        ms.shield_mode.store(true, Ordering::Release);
        (client, dir)
    }

    /// Cache `peer`'s `device` the way a normal-mode send does: a session.
    /// One identity per user, shared by all its devices.
    async fn cache_device(
        ms: &MessageService,
        peer: &UserId,
        device: u32,
        rng: &mut StdRng,
    ) -> libsignal_protocol::InMemSignalProtocolStore {
        use libsignal_protocol::{GenericSignedPreKey, KyberPreKeyStore, SignedPreKeyStore};
        let peer_identity =
            IdentityKeyPair::generate(&mut StdRng::seed_from_u64(peer.as_uuid().as_u128() as u64));
        let signed = KeyPair::generate(rng);
        let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, rng);
        let signed_sig = peer_identity
            .private_key()
            .calculate_signature(&signed.public_key.serialize(), rng)
            .unwrap();
        let kyber_sig = peer_identity
            .private_key()
            .calculate_signature(&kyber.public_key.serialize(), rng)
            .unwrap();
        let device_id = DeviceId::new(device as u8).unwrap();
        let registration = rng.random::<u32>() & 0x3fff;
        let signed_id = rng.random::<u32>().into();
        let kyber_id = rng.random::<u32>().into();
        let bundle = PreKeyBundle::new(
            registration,
            device_id,
            None,
            signed_id,
            signed.public_key,
            signed_sig.to_vec(),
            kyber_id,
            kyber.public_key.clone(),
            kyber_sig.to_vec(),
            *peer_identity.identity_key(),
        )
        .unwrap();
        let mut remote =
            libsignal_protocol::InMemSignalProtocolStore::new(peer_identity, registration).unwrap();
        let now = libsignal_protocol::Timestamp::from_epoch_millis(
            crate::util::time::TimestampNanos::now().as_u64() / 1_000_000,
        );
        remote
            .save_signed_pre_key(
                signed_id,
                &libsignal_protocol::SignedPreKeyRecord::new(signed_id, now, &signed, &signed_sig),
            )
            .await
            .unwrap();
        remote
            .save_kyber_pre_key(
                kyber_id,
                &libsignal_protocol::KyberPreKeyRecord::new(kyber_id, now, &kyber, &kyber_sig),
            )
            .await
            .unwrap();
        ms.signal_service
            .actor
            .establish_session(ProtocolAddress::new(peer.to_string(), device_id), bundle)
            .await
            .expect("session");
        remote
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

        let sent = ms
            .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
            .await;
        assert!(
            matches!(sent, Err(MessageError::NotReachablePrivately(p)) if p == friend),
            "got {sent:?}"
        );
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
        assert!(
            crate::mailbox::dm::DmConversationManager::new(ms.database.clone())
                .load_conversation(&friend)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn public_core_shield_refusal_releases_admission_for_later_reachable_preparation() {
        use crate::state::ActiveConversation;
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        client.auth_state().set_logged_in(me, 1, "me".into());
        let conversation = ActiveConversation::ServerDm { peer_id: friend };
        let refused = client.send_text(conversation, "first").await.unwrap();
        assert!(!refused.bootstrap_initiated);
        assert!(refused.send_error.is_some());
        assert_eq!(refused.message.unwrap().delivery_status, "failed");
        client.ensure_send_available(conversation).unwrap();
        assert!(
            client
                .messages()
                .pending_dms
                .drain(&friend)
                .unwrap()
                .is_empty()
        );

        cache_device(
            client.messages(),
            &friend,
            1,
            &mut StdRng::seed_from_u64(19),
        )
        .await;
        let mut auth = client.messages().auth_client();
        assert_eq!(
            client
                .messages()
                .bootstrap_devices(&mut auth, &friend)
                .await
                .unwrap(),
            vec![1]
        );
        seed_tokens(client.messages(), 2);
        // A later public send now reaches actual mailbox bootstrap. The offline
        // endpoint fails after its boundary, so admission must stay uncertain.
        let later = client.send_text(conversation, "second").await.unwrap();
        assert_eq!(later.message.unwrap().delivery_status, "sending");
        assert!(client.ensure_send_available(conversation).is_err());
    }

    // Local-only protocol fixture: generates a real sender/server certificate
    // under the repository's published development root. Never uses a keyring,
    // deployment key, live server, device or credential.
    #[cfg(feature = "dev-trust-root")]
    fn fixture_sender_certificate(
        client: &ChatterClient,
        me: UserId,
        phone: Option<String>,
    ) -> libsignal_protocol::SenderCertificate {
        use base64::Engine as _;
        use libsignal_protocol::{PrivateKey, SenderCertificate, ServerCertificate, Timestamp};
        let ms = client.messages();
        let mut rng = StdRng::seed_from_u64(0xCA9);
        // Same public test fixture as sealed_sender_decrypt_labels_unratcheted_plaintext.
        let root = PrivateKey::deserialize(
            &base64::engine::general_purpose::STANDARD
                .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
                .unwrap(),
        )
        .unwrap();
        let signer = KeyPair::generate(&mut rng);
        let server = ServerCertificate::new(1, signer.public_key, &root, &mut rng).unwrap();
        let (_, identity) = ms
            .signal_service
            .key_store
            .get_local_identity()
            .unwrap()
            .unwrap();
        let identity = IdentityKeyPair::try_from(identity.as_slice()).unwrap();
        let expiration = crate::util::time::TimestampNanos::now().as_u64() + 86_400_000_000_000;
        let cert = SenderCertificate::new(
            me.to_string(),
            phone,
            *identity.public_key(),
            DeviceId::new(1).unwrap(),
            Timestamp::from_epoch_millis(expiration / 1_000_000),
            server,
            &signer.private_key,
            &mut rng,
        )
        .unwrap();
        cert
    }

    #[cfg(feature = "dev-trust-root")]
    fn cache_fixture_sender_certificate(
        client: &ChatterClient,
        me: UserId,
    ) -> libsignal_protocol::SenderCertificate {
        use base64::Engine as _;
        let ms = client.messages();
        let cert = fixture_sender_certificate(client, me, None);
        let expiration = cert.expiration().unwrap().epoch_millis() * 1_000_000;
        ms.settings_repo
            .set_string(
                keys::SENDER_CERTIFICATE,
                &base64::engine::general_purpose::STANDARD.encode(cert.serialized().unwrap()),
            )
            .unwrap();
        ms.settings_repo
            .set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &expiration)
            .unwrap();
        ms.settings_repo
            .set(keys::SENDER_CERTIFICATE_SERVER_KEY_ID, &1u32)
            .unwrap();
        cert
    }

    #[cfg(feature = "dev-trust-root")]
    async fn open_fixture_sealed(
        remote: &mut libsignal_protocol::InMemSignalProtocolStore,
        alice: &ProtocolAddress,
        bob: &ProtocolAddress,
        envelope: &[u8],
    ) -> (Vec<u8>, libsignal_protocol::CiphertextMessageType) {
        use libsignal_protocol::{
            CiphertextMessage, CiphertextMessageType, PreKeySignalMessage, SignalMessage,
        };
        let usmc =
            libsignal_protocol::sealed_sender_decrypt_to_usmc(envelope, &remote.identity_store)
                .await
                .unwrap();
        let kind = usmc.msg_type().unwrap();
        let message = match kind {
            CiphertextMessageType::PreKey => CiphertextMessage::PreKeySignalMessage(
                PreKeySignalMessage::try_from(usmc.contents().unwrap()).unwrap(),
            ),
            CiphertextMessageType::Whisper => CiphertextMessage::SignalMessage(
                SignalMessage::try_from(usmc.contents().unwrap()).unwrap(),
            ),
            _ => panic!("unexpected sealed message type"),
        };
        let plain = libsignal_protocol::message_decrypt(
            &message,
            alice,
            bob,
            &mut remote.session_store,
            &mut remote.identity_store,
            &mut remote.pre_key_store,
            &remote.signed_pre_key_store,
            &mut remote.kyber_pre_key_store,
            &mut StdRng::seed_from_u64(17),
        )
        .await
        .unwrap();
        (plain, kind)
    }

    #[cfg(feature = "dev-trust-root")]
    #[tokio::test]
    async fn actual_sealed_batch_checks_fresh_kyber_and_established_devices_before_commit() {
        use crate::crypto::signal::store::StagedSignalStore;
        use crate::crypto::{SealedSenderManager, SignalStore};
        use crate::services::gateway::submission::track_submission;
        use libsignal_protocol::CiphertextMessageType;
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        let mut rng = StdRng::seed_from_u64(0xC500);
        let mut remote = cache_device(ms, &friend, 1, &mut rng).await;
        cache_device(ms, &friend, 2, &mut rng).await;
        let cert = cache_fixture_sender_certificate(&client, me);
        let alice = ProtocolAddress::new(me.to_string(), DeviceId::new(1).unwrap());
        let bob = ProtocolAddress::new(friend.to_string(), DeviceId::new(1).unwrap());
        let fresh = ProtocolAddress::new(friend.to_string(), DeviceId::new(2).unwrap());
        // Establish device 1 with actual encrypted request/reply; device 2 keeps
        // its unacknowledged fresh Kyber session. No transport or service runs.
        let first = ms
            .signal_service
            .actor
            .encrypt_sealed_bounded_batch(
                crate::network::validation::protovalidate::test_plaintext("handshake"),
                cert.clone(),
                vec![bob.clone()],
                None,
            )
            .await
            .unwrap();
        let (plain, kind) = open_fixture_sealed(&mut remote, &alice, &bob, &first[0].1).await;
        assert_eq!(
            plain,
            crate::network::validation::protovalidate::test_plaintext("handshake")
        );
        assert_eq!(kind, CiphertextMessageType::PreKey);
        let reply = libsignal_protocol::message_encrypt(
            b"ack",
            &alice,
            &bob,
            &mut remote.session_store,
            &mut remote.identity_store,
            std::time::SystemTime::now(),
            &mut rng,
        )
        .await
        .unwrap();
        assert_eq!(
            ms.signal_service
                .actor
                .decrypt_identified(reply.serialize().to_vec(), bob.clone())
                .await
                .unwrap(),
            b"ack"
        );
        let base = SignalStore::new(ms.signal_service.key_store.clone());
        let before = ms
            .signal_service
            .key_store
            .list_sessions_for_user(&friend.to_string())
            .unwrap();
        // Compare actual libsignal envelopes from independent disposable
        // overlays; find a body fitting device 1 but not fresh Kyber device 2.
        let mut near_cap = None;
        for size in (6000..8000).step_by(128) {
            let content = crate::services::build_data_message_content(&"N".repeat(size), vec![], 1);
            let bytes = prost::Message::encode_to_vec(&content);
            let mut lengths = vec![];
            for address in [&bob, &fresh] {
                let mut stage = StagedSignalStore::new(base.clone());
                let mut identities = stage.clone();
                lengths.push(
                    SealedSenderManager::encrypt(
                        &bytes,
                        &cert,
                        address,
                        &mut stage,
                        &mut identities,
                    )
                    .await
                    .unwrap()
                    .len(),
                );
            }
            if lengths[0] <= 8192 && lengths[1] > 8192 {
                near_cap = Some(bytes);
                break;
            }
        }
        let bytes = near_cap
            .expect("established and fresh Kyber wrappers must exercise mixed cap admission");
        let (result, started, _) = track_submission(
            ms.signal_service.actor.encrypt_sealed_bounded_batch(
                bytes,
                cert.clone(),
                vec![bob.clone(), fresh],
                None,
            ),
            None,
        )
        .await;
        assert!(matches!(
            result,
            Err(crate::error::ServiceError::Validation(_))
        ));
        assert!(
            !started,
            "later oversized device must not admit earlier device"
        );
        assert_eq!(
            before,
            ms.signal_service
                .key_store
                .list_sessions_for_user(&friend.to_string())
                .unwrap()
        );
        // A legitimate 6000-byte Content on the established session still
        // encrypts, decrypts byte-exact and marks the original future boundary.
        let content = crate::services::build_data_message_content(&"界".repeat(2000), vec![], 1);
        let bytes = prost::Message::encode_to_vec(&content);
        let (result, started, _) = track_submission(
            ms.signal_service.actor.encrypt_sealed_bounded_batch(
                bytes.clone(),
                cert,
                vec![bob.clone()],
                None,
            ),
            None,
        )
        .await;
        assert!(started);
        let accepted = result.unwrap();
        assert!(accepted[0].1.len() <= 8192);
        let (plain, kind) = open_fixture_sealed(&mut remote, &alice, &bob, &accepted[0].1).await;
        assert_eq!(plain, bytes);
        assert_eq!(kind, CiphertextMessageType::Whisper);
    }

    /// Reach the literal server cap with a real wrapped session and variable
    /// serialized certificate, rather than testing a guessed plaintext limit.
    #[cfg(feature = "dev-trust-root")]
    #[tokio::test]
    async fn actual_sealed_envelope_exact_cap_is_admitted_and_cap_plus_one_is_not() {
        use crate::crypto::signal::store::StagedSignalStore;
        use crate::crypto::{SealedSenderManager, SignalStore};
        use crate::services::gateway::submission::track_submission;
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_device(ms, &friend, 1, &mut StdRng::seed_from_u64(0x8192)).await;
        let address = ProtocolAddress::new(friend.to_string(), DeviceId::new(1).unwrap());
        let base = SignalStore::new(ms.signal_service.key_store.clone());
        let small = crate::network::validation::protovalidate::test_plaintext("small");
        for target in [8192, 8193] {
            let before = ms
                .signal_service
                .key_store
                .list_sessions_for_user(&friend.to_string())
                .unwrap();
            let baseline = {
                let cert = fixture_sender_certificate(&client, me, None);
                let mut staged = StagedSignalStore::new(base.clone());
                let mut identities = staged.clone();
                SealedSenderManager::encrypt(&small, &cert, &address, &mut staged, &mut identities)
                    .await
                    .unwrap()
                    .len()
            };
            let gap = target - baseline;
            let mut exact = None;
            // Certificate length prefixes add a few bytes; vary the actual
            // signed fixture instead of inventing an overhead allowance.
            for extra in gap.saturating_sub(32)..=gap {
                let cert = fixture_sender_certificate(&client, me, Some("1".repeat(extra)));
                let mut staged = StagedSignalStore::new(base.clone());
                let mut identities = staged.clone();
                let wrapped = SealedSenderManager::encrypt(
                    &small,
                    &cert,
                    &address,
                    &mut staged,
                    &mut identities,
                )
                .await
                .unwrap();
                if wrapped.len() == target {
                    exact = Some(cert);
                    break;
                }
            }
            let cert = exact.expect("actual envelope must hit the exact cap boundary");
            let (result, started, _) = track_submission(
                ms.signal_service.actor.encrypt_sealed_bounded_batch(
                    small.clone(),
                    cert,
                    vec![address.clone()],
                    None,
                ),
                None,
            )
            .await;
            if target == 8192 {
                assert!(started);
                assert_eq!(result.unwrap()[0].1.len(), 8192);
            } else {
                assert!(!started);
                assert!(matches!(
                    result,
                    Err(crate::error::ServiceError::Validation(_))
                ));
                assert_eq!(
                    before,
                    ms.signal_service
                        .key_store
                        .list_sessions_for_user(&friend.to_string())
                        .unwrap()
                );
            }
        }
    }

    /// Public send, real SQLite admission, actual PQXDH sealed envelope and
    /// sender certificate: near-cap refusal must leave BOTH ratchets intact.
    #[cfg(feature = "dev-trust-root")]
    #[tokio::test]
    async fn public_sealed_envelope_refusal_releases_admission_without_advancing_devices() {
        use crate::state::ActiveConversation;
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        ms.shield_mode.store(false, Ordering::Release);
        client.auth_state().set_logged_in(me, 1, "me".into());
        let mut rng = StdRng::seed_from_u64(0xC49);
        for device in [1, 2] {
            cache_device(ms, &friend, device, &mut rng).await;
        }
        ms.signal_service
            .unrestricted_access
            .lock()
            .unwrap()
            .insert(friend.to_string(), false);
        cache_fixture_sender_certificate(&client, me);
        let before = ms
            .signal_service
            .key_store
            .list_sessions_for_user(&friend.to_string())
            .unwrap();
        let conversation = ActiveConversation::ServerDm { peer_id: friend };
        // Below 8192 in encoded Content, but the real fresh Kyber wrapper is over.
        let content = ms
            .server_dm_content(&friend, &"N".repeat(8000), vec![], 1, None)
            .unwrap()
            .0;
        assert!(prost::Message::encoded_len(&content) < 8192);
        let refused = client
            .send_text(conversation, &"N".repeat(8000))
            .await
            .unwrap();
        assert!(matches!(
            refused.send_error,
            Some(MessageError::InvalidContent(_))
        ));
        assert_eq!(refused.message.unwrap().delivery_status, "failed");
        client.ensure_send_available(conversation).unwrap();
        let after = ms
            .signal_service
            .key_store
            .list_sessions_for_user(&friend.to_string())
            .unwrap();
        assert_eq!(
            before, after,
            "no device ratchet may advance on size refusal"
        );
        // Small real encryption is admitted. Its deliberately offline deposit
        // loses confirmation; the outbox keeps the exact envelope to replay.
        let small = client
            .send_text(conversation, "small after cap refusal")
            .await
            .unwrap();
        assert!(small.send_error.is_none());
        let small = small.message.unwrap();
        assert_eq!(small.delivery_status, "sending");
        assert_ne!(
            before,
            ms.signal_service
                .key_store
                .list_sessions_for_user(&friend.to_string())
                .unwrap()
        );
        let envelope = |id: &crate::domain::ids::MessageId| -> Option<Vec<u8>> {
            ms.database
                .read_connection()
                .unwrap()
                .query_row(
                    "SELECT request FROM dm_outbox WHERE message_id=?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let retained = envelope(&small.id).expect("exact envelope stored before the deposit");
        // A replayable envelope is not a reason to hold the conversation: a
        // new send is a new message, and the queued one is never re-encrypted.
        client.ensure_send_available(conversation).unwrap();
        let another = client.send_text(conversation, "another").await.unwrap();
        assert_eq!(another.message.unwrap().delivery_status, "sending");
        assert_eq!(envelope(&small.id), Some(retained));
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
        assert_eq!(
            ms.bootstrap_devices(&mut auth, &friend).await.unwrap(),
            vec![6, 3, 1]
        );

        ms.retired_peer_devices
            .lock()
            .insert((friend, 6), std::time::Instant::now());
        assert_eq!(
            ms.bootstrap_devices(&mut auth, &friend).await.unwrap(),
            vec![3, 1]
        );
    }

    /// Cache a sender certificate for `me` (device 1) that chains to the dev
    /// trust root this test build pins — its private key is committed in
    /// `deploy/vault/init-and-unseal.sh` — so sealing needs no RPC.
    fn cache_sender_certificate(ms: &MessageService, me: &UserId) {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
        use libsignal_protocol::{PrivateKey, SenderCertificate, ServerCertificate, Timestamp};
        let rng = &mut StdRng::seed_from_u64(0xCE27);
        let root = PrivateKey::deserialize(
            &BASE64
                .decode("ELMk0o1RlhsHCk8vQ/bWbtRDeQvcC4PuN8+U1Xg90Ek=")
                .unwrap(),
        )
        .unwrap();
        let server = KeyPair::generate(rng);
        let server_cert = ServerCertificate::new(1, server.public_key, &root, rng).unwrap();
        let identity = IdentityKeyPair::generate(&mut StdRng::seed_from_u64(7));
        let far_future = Timestamp::from_epoch_millis(u64::MAX / 2);
        let cert = SenderCertificate::new(
            me.to_string(),
            None,
            *identity.public_key(),
            DeviceId::new(1).unwrap(),
            far_future,
            server_cert,
            &server.private_key,
            rng,
        )
        .unwrap();
        ms.settings_repo
            .set_string(
                keys::SENDER_CERTIFICATE,
                &BASE64.encode(cert.serialized().unwrap()),
            )
            .unwrap();
        ms.settings_repo
            .set(keys::SENDER_CERTIFICATE_REFRESH_AFTER, &(u64::MAX / 2))
            .unwrap();
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
            &ms.signal_service,
            &mut auth,
            &friend,
            6,
            &crate::network::validation::protovalidate::test_plaintext("queued"),
            &[7u8; 32],
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

        let sent = ms
            .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
            .await;
        match sent {
            Err(MessageError::ShieldNotReady) => {}
            other => panic!("expected the empty-batch refusal, got {other:?}"),
        }
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 0);
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
    }

    /// The composing path must retain an empty-batch refusal as a failed,
    /// retryable bubble, rather than reporting it sent or losing the content.
    #[tokio::test]
    async fn shield_empty_token_send_keeps_a_failed_bubble_for_retry() {
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(9)).await;

        let result = ms
            .send_server_dm(&friend, "keep this", vec![], 12345, None)
            .await
            .expect("local echo persists even when delivery fails");
        assert!(matches!(
            result.send_error,
            Some(MessageError::ShieldNotReady)
        ));
        assert!(!result.bootstrap_initiated);
        let row = result.message.expect("outgoing bubble");
        let stored = ms.message_repo.get(&row.id).unwrap().unwrap();
        assert_eq!(stored.delivery_status, "failed");
        assert_eq!(
            stored.failure_reason.as_deref(),
            Some(super::super::core::SHIELD_WAITING_FOR_TOKENS)
        );
        assert_eq!(stored.as_text(), Some("keep this"));
        assert_eq!(ms.token_manager.available_token_count().unwrap(), 0);

        // Retry must keep the original row and its failed status if the pool
        // is still empty. No token request may be coupled to either attempt.
        assert!(ms.retry_server_dm(&row.id).await.is_err());
        let retried = ms.message_repo.get(&row.id).unwrap().unwrap();
        assert_eq!(retried.delivery_status, "failed");
        assert_eq!(retried.sent_at, stored.sent_at);
        assert_eq!(retried.as_text(), Some("keep this"));
    }

    /// Deterministic retry boundary: the real composing path parks locally;
    /// a simulated scheduled refill does not wake a send, and the independent
    /// retry tick submits the original row once. The callback stands for the
    /// transport owner (production uses the client's durable retry journal).
    #[tokio::test(start_paused = true)]
    async fn shield_parked_post_waits_for_independent_retry_after_refill() {
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        client.auth_state().set_logged_in(me, 1, "me".into());
        let conversation = crate::state::ActiveConversation::ServerDm { peer_id: friend };
        let ms = client.messages();
        cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(97)).await;
        let result = client
            .send_text(conversation, "park exactly once")
            .await
            .unwrap();
        client.ensure_send_available(conversation).unwrap();
        assert!(matches!(
            result.send_error,
            Some(MessageError::ShieldNotReady)
        ));
        let row = result.message.unwrap();
        assert_eq!(
            super::super::display_delivery_status(
                &row.delivery_status,
                row.failure_reason.as_deref()
            ),
            "parked"
        );
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
        assert_eq!(
            super::super::shield_retry::retry_parked_with(ms, me, |_| async {
                panic!("empty pool must not dispatch")
            })
            .await
            .unwrap(),
            0
        );
        assert!(matches!(
            client.retry_failed_message(&row.id.to_string()).await,
            Err(crate::services::gateway::outgoing::SendError::Message(
                MessageError::ShieldNotReady
            ))
        ));

        client.ensure_send_available(conversation).unwrap();
        // Start the independent retry timer before simulating a refill.
        let mut retry_tick = Box::pin(crate::app::core_services::jittered_sleep());
        assert!(futures::poll!(&mut retry_tick).is_pending());
        seed_tokens(ms, 2);
        assert_eq!(
            ms.message_repo
                .get(&row.id)
                .unwrap()
                .unwrap()
                .delivery_status,
            "failed"
        );
        tokio::time::advance(std::time::Duration::from_secs(299)).await;
        assert!(
            futures::poll!(&mut retry_tick).is_pending(),
            "refill cannot wake the retry timer"
        );
        tokio::time::advance(std::time::Duration::from_secs(601)).await;
        retry_tick.await;
        let delivered = std::sync::atomic::AtomicUsize::new(0);
        let attempt = |id| {
            assert_eq!(id, row.id);
            assert_eq!(
                ms.message_repo.get(&id).unwrap().unwrap().as_text(),
                Some("park exactly once")
            );
            delivered.fetch_add(1, Ordering::SeqCst);
            ms.finish_retry_row(&id, "sent", None).unwrap();
            std::future::ready(())
        };
        assert_eq!(
            super::super::shield_retry::retry_parked_with(ms, me, attempt)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            super::super::shield_retry::retry_parked_with(ms, me, |_| async {
                panic!("confirmed row must not repeat")
            })
            .await
            .unwrap(),
            0
        );
        assert_eq!(delivered.load(Ordering::SeqCst), 1);
        assert_eq!(
            ms.token_manager.available_token_count().unwrap(),
            2,
            "tick does not request or claim tokens; the transport owner spends them"
        );
    }

    /// Our side of a conversation whose peer mailbox is gone (expired, or
    /// dropped as by v069): a live mailbox of ours and an established key,
    /// nothing to send to. Returns our mailbox public key.
    fn seed_half_open(
        ms: &MessageService,
        peer: &UserId,
        key: &crate::mailbox::dm::ConversationKey,
        pk: u8,
    ) -> [u8; 32] {
        let mailbox = crate::mailbox::dm::DmMailbox {
            id: format!("mb-{pk}"),
            public_key: [pk; 32],
            private_key: [pk; 64],
            encryption_private_key: [pk; 32],
            signature: vec![],
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
    /// that has nothing to send. With an empty token pool, park before any
    /// REQUEST submission; the independent retry can ask once tokens exist.
    /// Never queue silently behind a bootstrap that has not been requested.
    #[tokio::test]
    async fn higher_uuid_without_a_peer_mailbox_waits_for_prefetched_tokens() {
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(11)).await;
        cache_sender_certificate(ms, &me);
        let (mut ours, mut theirs) = (
            crate::mailbox::dm::ConversationKey::new(),
            crate::mailbox::dm::ConversationKey::new(),
        );
        ours.derive_shared_secret(theirs.our_ephemeral_public)
            .unwrap();
        theirs
            .derive_shared_secret(ours.our_ephemeral_public)
            .unwrap();
        seed_half_open(ms, &friend, &ours, 5);

        let sent = ms
            .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
            .await;
        assert!(
            matches!(sent, Err(MessageError::ShieldNotReady)),
            "got {sent:?}"
        );
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
    }

    /// F51 recovery: the lower-UUID side re-announces its live mailbox and
    /// key (no token, no new mailbox), and the higher side — established on
    /// that same key but missing the peer mailbox — takes it: it records the
    /// mailbox for the announcing device and can send again. F53: a later
    /// re-bootstrap with a new ephemeral (the lower side's mailbox expired)
    /// replaces the working conversation's keys rather than being refused.
    #[tokio::test]
    async fn half_open_conversation_recovers_by_re_announce() {
        use crate::error::MailboxError;
        use crate::mailbox::dm::{ConversationKey, DmBootstrap, DmConversationManager};
        use prost::Message as _;

        let lower = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let higher = UserId::from_uuid(uuid::Uuid::max());
        let (lower_client, _d1) = shield_client(lower);
        let (higher_client, _d2) = shield_client(higher);
        let (lo, hi) = (lower_client.messages(), higher_client.messages());

        let (mut lo_key, mut hi_key) = (ConversationKey::new(), ConversationKey::new());
        lo_key
            .derive_shared_secret(hi_key.our_ephemeral_public)
            .unwrap();
        hi_key
            .derive_shared_secret(lo_key.our_ephemeral_public)
            .unwrap();
        let lo_mailbox = seed_half_open(lo, &higher, &lo_key, 5);
        let hi_mailbox = seed_half_open(hi, &lower, &hi_key, 6);

        // Lower side: re-announce, with an empty token batch (a fresh mailbox would fail).
        let announced = DmBootstrap::new(lo.database.clone())
            .initiate_dm(higher, &mut lo.grpc_pool.anonymous_mailbox_client())
            .await
            .expect("re-announce needs no token and no RPC");
        assert_eq!(announced.our_mailbox.public_key, lo_mailbox);
        assert_eq!(announced.our_ephemeral_public, lo_key.our_ephemeral_public);
        let content =
            crate::proto::content_v1::Content::decode(announced.bootstrap_message.as_slice())
                .unwrap();
        let Some(crate::proto::content_v1::content::Content::KeyExchange(kx)) = content.content
        else {
            panic!("bootstrap MESSAGE is a KeyExchange");
        };

        // Higher side takes it from the lower side's device 3.
        let bootstrap = DmBootstrap::new(hi.database.clone());
        let response = bootstrap
            .handle_incoming_bootstrap(
                lower,
                3,
                &kx.ephemeral_pk,
                &mut hi.grpc_pool.anonymous_mailbox_client(),
            )
            .await
            .expect("accepted")
            .expect("answered with our mailbox");
        assert_eq!(response.our_mailbox_public_key, hi_mailbox);
        let conv = DmConversationManager::new(hi.database.clone())
            .load_conversation(&lower)
            .unwrap()
            .unwrap();
        assert!(conv.is_ready_to_send());
        let peer_mailbox = &conv.peer_mailboxes[0];
        assert_eq!(
            (peer_mailbox.public_key, peer_mailbox.device_id),
            (lo_mailbox, 3)
        );
        assert_eq!(
            conv.device_key(peer_mailbox).unwrap().send_key(),
            lo_key.recv_key()
        );

        // F53: the lower side's mailbox expires, so it re-bootstraps with a
        // fresh mailbox and ephemeral. The higher side still holds a working
        // conversation, and takes the new keys anyway: the sender is the
        // authenticated peer (the Signal layer refuses any other identity).
        let mut new_lo_key = ConversationKey::new();
        let new_lo_mailbox = [0x5Au8; 32];
        let (_, new_lo_encryption_pk) =
            crate::crypto::envelope_crypto::generate_mailbox_encryption_key();
        let rebootstrap = crate::mailbox::dm::bootstrap::MailboxAnnouncement {
            mailbox_pk: new_lo_mailbox,
            conversation_pk: new_lo_key.our_ephemeral_public,
            mailbox_encryption_pk: new_lo_encryption_pk,
        }
        .encode();
        let response = bootstrap
            .handle_incoming_bootstrap(
                lower,
                3,
                &rebootstrap,
                &mut hi.grpc_pool.anonymous_mailbox_client(),
            )
            .await
            .expect("re-bootstrap accepted")
            .expect("answered with our mailbox");
        assert_eq!(response.peer_mailbox_public_key, new_lo_mailbox);
        assert_eq!(response.peer_mailbox_encryption_key, new_lo_encryption_pk);
        new_lo_key
            .derive_shared_secret(response.our_ephemeral_public)
            .unwrap();
        let conv = DmConversationManager::new(hi.database.clone())
            .load_conversation(&lower)
            .unwrap()
            .unwrap();
        assert!(conv.is_ready_to_send());
        assert_eq!(
            conv.peer_mailboxes.len(),
            1,
            "device 3's half replaced, not added"
        );
        assert_eq!(conv.peer_mailboxes[0].public_key, new_lo_mailbox);
        let hi_key = conv.device_key(&conv.peer_mailboxes[0]).unwrap();
        assert_eq!(hi_key.send_key(), new_lo_key.recv_key());
        assert_ne!(
            hi_key.send_key(),
            lo_key.recv_key(),
            "the old secret is gone"
        );

        // A bootstrap from any other identity is filed under that identity:
        // it cannot touch the conversation with `lower`.
        let mallory = UserId::from_uuid(uuid::Uuid::from_u128(2));
        let hostile = crate::mailbox::dm::bootstrap::MailboxAnnouncement {
            mailbox_pk: [0x66u8; 32],
            conversation_pk: ConversationKey::new().our_ephemeral_public,
            mailbox_encryption_pk: [0x67u8; 32],
        }
        .encode();
        match bootstrap
            .handle_incoming_bootstrap(
                mallory,
                3,
                &hostile,
                &mut hi.grpc_pool.anonymous_mailbox_client(),
            )
            .await
        {
            // No conversation with mallory: needs a fresh mailbox, and the token batch is empty.
            Err(MailboxError::NoTokensAvailable) => {}
            other => panic!(
                "expected the empty token batch, got {:?}",
                other.map(|r| r.is_some())
            ),
        }
        let conv = DmConversationManager::new(hi.database.clone())
            .load_conversation(&lower)
            .unwrap()
            .unwrap();
        assert_eq!(conv.peer_mailboxes[0].public_key, new_lo_mailbox);
        assert_eq!(
            conv.device_key(&conv.peer_mailboxes[0]).unwrap().send_key(),
            new_lo_key.recv_key()
        );
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
    /// server URL, so the batched deposit fails after its credential is spent:
    /// four independently encrypted devices, one batch credential.
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

        let before: Vec<_> = [2, 6, 7, 8]
            .into_iter()
            .map(|device| {
                ms.signal_service
                    .key_store
                    .load_session(&format!("{friend}:{device}"))
                    .unwrap()
                    .expect("cached device session")
            })
            .collect();

        let sent = ms
            .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
            .await;
        assert!(
            matches!(sent, Err(MessageError::DmError(ref m)) if m.contains("Failed to send bootstrap")),
            "got {sent:?}"
        );
        assert_eq!(
            ms.token_manager.available_token_count().unwrap(),
            9,
            "one credential covers the complete device batch"
        );
        for (device, original) in [2, 6, 7, 8].into_iter().zip(before) {
            let after = ms
                .signal_service
                .key_store
                .load_session(&format!("{friend}:{device}"))
                .unwrap()
                .expect("device session retained");
            assert_ne!(after, original, "REQUEST encrypted for device {device}");
        }
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
            .record_peer_device(
                &friend,
                6,
                [6u8; 32],
                ConversationKey::new().our_ephemeral_public,
            )
            .unwrap();
        let conv = conversations.load_conversation(&friend).unwrap().unwrap();
        assert!(conv.is_ready_to_send());
        seed_tokens(ms, 10);

        let mut auth = ms.auth_client();
        ms.owe_unready_devices(&mut auth, &friend, &conv, b"one", Some(&ms.grpc_pool))
            .await;
        assert_eq!(
            ms.token_manager.available_token_count().unwrap(),
            8,
            "7 and 8 asked"
        );
        ms.owe_unready_devices(&mut auth, &friend, &conv, b"two", Some(&ms.grpc_pool))
            .await;
        assert_eq!(
            ms.token_manager.available_token_count().unwrap(),
            6,
            "failed asks repeated"
        );
        for device in [7, 8] {
            conversations.mark_device_asked(&friend, device).unwrap();
        }
        ms.owe_unready_devices(&mut auth, &friend, &conv, b"three", Some(&ms.grpc_pool))
            .await;
        assert_eq!(
            ms.token_manager.available_token_count().unwrap(),
            6,
            "a delivered ask is not repeated"
        );

        assert!(
            conversations
                .drain_device_queue(&friend, 6)
                .unwrap()
                .is_empty()
        );
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
            .record_peer_device(
                &friend,
                8,
                [8u8; 32],
                ConversationKey::new().our_ephemeral_public,
            )
            .unwrap();
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());

        let mut auth = ms.auth_client();
        let flush =
            crate::inbox::streaming::dm_bootstrap_handler::send_pending_messages_after_bootstrap(
                friend,
                8,
                ms.database.clone(),
                &ms.signal_service,
                &mut auth,
                ms.grpc_pool.unidentified_channel(),
                ms,
            )
            .await;
        assert!(flush.is_none(), "nothing queued, nothing to announce");
        assert!(
            conversations
                .drain_device_queue(&friend, 8)
                .unwrap()
                .is_empty(),
            "owed copy taken for device 8"
        );
    }

    /// A DM queued before the Shield bootstrap finished and sent by the
    /// responder's flush records its receipt targets like any send: the
    /// peer device's Receipt naming the original DataMessage timestamp makes
    /// the bubble "delivered". The flush used to deposit without recording
    /// them, so that Receipt matched nothing and the bubble stayed "sent".
    /// Nothing is behind the server URL: the deposit fails, but the targets
    /// are captured before transport, as `send_dm_content` does.
    #[tokio::test]
    async fn flushed_pending_dm_is_marked_delivered_by_the_peer_receipt() {
        use crate::domain::ids::{ChannelId, DeviceId as Device};
        use crate::domain::message::{ConversationType, Message};
        use crate::mailbox::dm::{ConversationKey, DmConversationManager};
        use prost::Message as _;
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_sender_certificate(ms, &me);
        cache_device(ms, &friend, 6, &mut StdRng::seed_from_u64(0x5EC)).await;
        seed_half_open(ms, &friend, &ConversationKey::new(), 5);

        // Queued before the bootstrap, stored as a "sending" bubble.
        let sent_at = 1_700_000_000_123_456_789u64;
        let content = crate::services::build_text_content("first", sent_at);
        ms.pending_dms
            .queue(&friend, content.encode_to_vec(), vec![], None)
            .unwrap();
        let mut message = Message::new_text(
            ChannelId::from(friend),
            ConversationType::ServerDm,
            Some(me),
            "first".into(),
            sent_at,
        );
        message.delivery_status = "sending".into();
        ms.message_repo.insert_outgoing(&message).unwrap();

        // The peer's device 6 answers the bootstrap; the queue is flushed.
        DmConversationManager::new(ms.database.clone())
            .record_peer_device(
                &friend,
                6,
                [6u8; 32],
                ConversationKey::new().our_ephemeral_public,
            )
            .unwrap();
        let mut auth = ms.auth_client();
        crate::inbox::streaming::dm_bootstrap_handler::send_pending_messages_after_bootstrap(
            friend,
            6,
            ms.database.clone(),
            &ms.signal_service,
            &mut auth,
            ms.grpc_pool.unidentified_channel(),
            ms,
        )
        .await
        .expect("one message was queued");

        // Device 6's delivery Receipt names the original timestamp.
        ms.apply_peer_receipt(
            ChannelId::from(friend),
            ConversationType::ServerDm,
            friend,
            Device(6),
            1,
            &[sent_at],
        )
        .unwrap();
        let stored = ms.message_repo.get(&message.id).unwrap().unwrap();
        assert_eq!(stored.delivery_status, "delivered");
    }

    /// Initiator side (S25, 2026-10-03): a DM queued before the bootstrap and
    /// sent by `send_pending_messages` is stored "delivered" by the peer
    /// device's Receipt, and the flush's later `DmBootstrapCompleted` "sent"
    /// write does not undo it. The stored state was right on the phone too;
    /// the bubble missed the update because it carried a synthetic id
    /// (`client::queued_bubble_id`).
    #[tokio::test]
    async fn initiator_flush_completion_does_not_regress_a_delivered_pending_dm() {
        use crate::domain::ids::{ChannelId, DeviceId as Device};
        use crate::domain::message::{ConversationType, Message};
        use crate::mailbox::dm::{ConversationKey, DmConversationManager};
        use prost::Message as _;
        let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let friend = UserId::from_uuid(uuid::Uuid::max());
        let (client, _dir) = shield_client(me);
        let ms = client.messages();
        cache_sender_certificate(ms, &me);
        cache_device(ms, &friend, 6, &mut StdRng::seed_from_u64(0x1417)).await;
        seed_half_open(ms, &friend, &ConversationKey::new(), 5);

        let sent_at = 1_700_000_000_987_654_321u64;
        let content = crate::services::build_text_content("shield over tor", sent_at);
        ms.pending_dms
            .queue(&friend, content.encode_to_vec(), vec![], None)
            .unwrap();
        let mut message = Message::new_text(
            ChannelId::from(friend),
            ConversationType::ServerDm,
            Some(me),
            "shield over tor".into(),
            sent_at,
        );
        message.delivery_status = "sending".into();
        ms.message_repo.insert_outgoing(&message).unwrap();

        // The RESPONSE from device 6 arrived; the initiator flushes.
        DmConversationManager::new(ms.database.clone())
            .record_peer_device(
                &friend,
                6,
                [6u8; 32],
                ConversationKey::new().our_ephemeral_public,
            )
            .unwrap();
        let mut auth = ms.auth_client();
        let _ = ms
            .send_pending_messages(&mut auth, &friend, &ms.mailbox_manager)
            .await;

        ms.apply_peer_receipt(
            ChannelId::from(friend),
            ConversationType::ServerDm,
            friend,
            Device(6),
            1,
            &[sent_at],
        )
        .unwrap();
        assert_eq!(
            ms.message_repo
                .get(&message.id)
                .unwrap()
                .unwrap()
                .delivery_status,
            "delivered"
        );

        // What the `DmBootstrapCompleted` handler then persists.
        ms.set_friend_dm_delivery_status(&ChannelId::from(friend), &[sent_at], "sent")
            .unwrap();
        assert_eq!(
            ms.message_repo
                .get(&message.id)
                .unwrap()
                .unwrap()
                .delivery_status,
            "delivered"
        );
    }
}
