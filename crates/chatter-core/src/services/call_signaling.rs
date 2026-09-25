//! Call signaling service.
//!
//! Handles the Signal Protocol side of call setup: building call key exchange
//! protos and sending them via pairwise DM (sealed sender). This keeps
//! protocol details out of the UI layer.
//!
//! # DM Calls (1:1)
//! Uses ephemeral X25519 key exchange via `CallKeyExchange` protos.
//! Offer → Answer → both derive shared secret → AES key for media.
//!
//! # Group Calls
//! Uses a randomly generated AES-256 key distributed to all members
//! via pairwise sealed sender DMs (`GroupCallKey` proto).

use std::sync::Arc;

use parking_lot::Mutex;

#[cfg(feature = "media")]
use crate::crypto::call_key_provider::EphemeralCallKey;
use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
#[cfg(feature = "media")]
use crate::domain::call::{AnonCallAnswerOutcome, AnonCallSetupResult, CallSetupResult};
#[cfg(feature = "media")]
use crate::domain::call::CallType;
use crate::domain::call::PendingDmCall;

#[cfg(feature = "media")]
use crate::domain::ids::CallId;
use crate::domain::ids::{ChannelId, UserId};
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
#[cfg(feature = "media")]
use crate::services::call_verification::CallVerification;
#[cfg(feature = "media")]
use crate::services::call_worker::{
    CallEndReason, CallHandle, CallWorkerHandle, OrchestratedCallKind, RING_TIMEOUT,
};
use crate::services::channel::ChannelService;
use crate::services::gateway::{
    build_call_key_offer, build_call_key_answer,
    build_call_hangup_content, build_group_call_key_content,
    CallHangupReason,
};
use crate::services::message::MessageService;
use crate::services::signal_service::SignalService;

/// Errors from call signaling operations.
#[derive(Debug, thiserror::Error)]
pub enum CallSignalingError {
    #[error("message send failed: {0}")]
    Send(#[from] crate::services::message::MessageError),
    #[error("channel error: {0}")]
    Channel(#[from] crate::services::channel::ChannelError),
    #[error("no DM peer found for channel {0}")]
    NoPeer(ChannelId),
    #[error("no pending DM call to complete")]
    NoPendingCall,
    #[error("call ID mismatch: expected {expected}, got {actual}")]
    CallIdMismatch { expected: String, actual: String },
    #[error("key derivation failed: {0}")]
    KeyDerivationFailed(String),
    #[error("call worker error: {0}")]
    CallWorker(String),
    #[error("anonymous call error: {0}")]
    AnonCall(String),
    #[error("no call room tokens available")]
    NoTokens,
    /// Mobile-path orchestration error (LiveKit setup, gRPC create/end call,
    /// JSON decode of server responses). Kept as a free-form string because
    /// the mobile methods bridge multiple subsystems and their error surfaces
    /// are shaped by the platform SDK rather than our typed domain errors.
    #[error("mobile call: {0}")]
    Mobile(String),
}

/// Handles call signaling (key exchange, hangup) over the Signal Protocol.
///
/// Every signaling message goes through `MessageService::send_friend_dm_content`,
/// so its transport follows the mode rule for friend DMs: Shield → the mailbox,
/// normal → sealed-sender streaming (a call needs signaling that arrives now,
/// not at the next mailbox poll).
#[derive(Clone)]
pub struct CallSignalingService {
    message_service: Arc<MessageService>,
    channel_service: Arc<ChannelService>,
    anon_dm_service: Arc<AnonDmService>,
    anon_group_service: Arc<AnonGroupService>,
    /// Pending outgoing DM call state (waiting for answer with peer's ephemeral key).
    pending_dm_call: Arc<Mutex<Option<PendingDmCall>>>,
}

impl CallSignalingService {
    pub fn new(
        message_service: Arc<MessageService>,
        channel_service: Arc<ChannelService>,
        anon_dm_service: Arc<AnonDmService>,
        anon_group_service: Arc<AnonGroupService>,
    ) -> Self {
        Self {
            message_service,
            channel_service,
            anon_dm_service,
            anon_group_service,
            pending_dm_call: Arc::new(Mutex::new(None)),
        }
    }

    /// Store a pending outgoing DM call.
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
        channel_id: &str,
    ) -> Result<(), CallSignalingError> {
        let content = build_call_key_offer(call_id, ephemeral_public_key, call_type, channel_id);
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
    /// Looks up the peer from the DM channel automatically.
    pub async fn send_call_hangup(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        reason: CallHangupReason,
    ) -> Result<(), CallSignalingError> {
        // For a channel/group DM there's a server DM channel to look the peer up
        // in. For a private (anon-derived) DM call there is no server channel —
        // the "channel_id" IS the peer's user_id — so fall back to that.
        let peer_id = match self.channel_service.get_dm_peer_local(channel_id)? {
            Some(p) => p,
            None => UserId::from_uuid(*channel_id.as_uuid()),
        };
        self.send_call_hangup_to(&peer_id, call_id, reason).await
    }

    /// Tell the caller we declined their 1:1 call. The self-sync copy of this
    /// hangup stops our other devices ringing (`build_call_self_sync`).
    pub async fn send_call_decline(
        &self,
        peer_id: &UserId,
        call_id: &str,
    ) -> Result<(), CallSignalingError> {
        self.send_call_hangup_to(peer_id, call_id, CallHangupReason::Declined).await
    }

    async fn send_call_hangup_to(
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
            Ok(_) => tracing::info!(peer_id = %peer_id, call_id, reason = ?reason, "Call hangup sent"),
            // Session still bootstrapping — the queued hangup will deliver, and the
            // peer sees the LiveKit disconnect regardless. Not fatal.
            Err(crate::services::message::MessageError::BootstrapInitiated) => {
                tracing::info!(peer_id = %peer_id, call_id, "Call hangup queued during DM bootstrap");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Redistribute a rotated group call key to the peers actually in the room.
    ///
    /// Unlike [`Self::distribute_group_call_key`] — which invites the whole
    /// channel to a call that is starting — rotation exists to lock a departed
    /// participant *out*. A peer who leaves the LiveKit room is still a channel
    /// member, so sending the rotated key to the membership list hands it
    /// straight back to the attacker the rotation targets. Recipients are the
    /// live room roster intersected with channel membership: room identities
    /// that are not entitled to the channel are dropped, never sent to.
    pub async fn redistribute_group_call_key(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        key: [u8; 32],
        epoch: u32,
        room_identities: &[String],
    ) -> Result<usize, CallSignalingError> {
        let current_user = self.channel_service.current_user_id()?;
        // Full membership, paginated: a member past the server's page size who
        // IS in the room used to be silently skipped here and left unable to
        // decrypt after rotation (OPEN-ITEMS §16).
        let members = self.channel_service.get_all_channel_members(channel_id).await?;
        let recipients = rotation_recipients(&members, room_identities, &current_user);
        let channel_id_str = channel_id.to_string();

        let mut sent_count = 0;
        for peer_id in &recipients {
            let content = build_group_call_key_content(call_id, key, epoch, &channel_id_str);
            if let Err(e) = self.message_service
                .send_friend_dm_content(peer_id, content)
                .await
            {
                tracing::warn!(
                    call_id, epoch, peer_id = %peer_id, error = %e,
                    "Failed to redistribute rotated group call key to participant"
                );
            } else {
                sent_count += 1;
            }
        }

        tracing::info!(
            call_id, epoch, sent_count,
            roster = room_identities.len(),
            members = members.len(),
            "Completed rotated group call key redistribution"
        );
        Ok(sent_count)
    }

    /// Distribute a group call encryption key to all channel members.
    ///
    /// Sends the key pairwise via sealed sender DM to each member
    /// (excluding ourselves). This ensures all participants have the
    /// same AES-256 key for E2EE media.
    pub async fn distribute_group_call_key(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        key: [u8; 32],
        epoch: u32,
    ) -> Result<usize, CallSignalingError> {
        let current_user = self.channel_service.current_user_id()?;
        // Full membership, paginated — a member past the server's page size
        // otherwise never receives the call key at all (OPEN-ITEMS §16).
        let members = self.channel_service.get_all_channel_members(channel_id).await?;
        let channel_id_str = channel_id.to_string();

        let mut sent_count = 0;
        for member in &members {
            if member.user_id == current_user {
                continue;
            }

            let content = build_group_call_key_content(call_id, key, epoch, &channel_id_str);
            if let Err(e) = self.message_service
                .send_friend_dm_content(&member.user_id, content)
                .await
            {
                tracing::warn!(
                    call_id, peer_id = %member.user_id, error = %e,
                    "Failed to distribute group call key to member"
                );
            } else {
                sent_count += 1;
            }
        }

        tracing::info!(call_id, sent_count, "Completed group call key distribution");
        Ok(sent_count)
    }

    // =========================================================================
    // DM Call Orchestration
    // =========================================================================

    /// Start a DM call: generate ephemeral key, create call on server, send offer, store pending state.
    ///
    /// Returns the call ID. The call is not yet connected — that happens when
    /// `complete_dm_call()` is called after the peer sends their answer.
    #[cfg(feature = "media")]
    pub async fn start_dm_call(
        &self,
        peer_id: &UserId,
        channel_id: ChannelId,
        call_type: CallType,
        call_worker: &CallWorkerHandle,
        user_id: UserId,
    ) -> Result<CallId, CallSignalingError> {
        // Generate ephemeral keypair for PFS
        let ephemeral_key = EphemeralCallKey::generate();
        let ephemeral_public = ephemeral_key.public_key_bytes();

        let is_video = matches!(call_type, CallType::Video);
        let call_type_str = match call_type {
            CallType::Voice => "Voice Call",
            CallType::Video => "Video Call",
            CallType::Group => "Group Call",
        };

        tracing::info!(
            peer_id = %peer_id,
            "Generated ephemeral key for DM call, sending offer"
        );

        // Create call on server with placeholder key (will update when answer arrives)
        let placeholder_key = [0u8; 32];
        let call = call_worker
            .initiate_call(channel_id, call_type_str, call_type, &placeholder_key, user_id)
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        tracing::info!("Created call on server: {:?}, waiting for peer's ephemeral key", call.id);

        // Send offer via sealed sender DM. `initiate_call` already connected
        // the LiveKit room and set the worker's active_call; if the offer
        // fails to send (e.g. first-contact DM bootstrap, or a network error)
        // we must roll that back. Otherwise the worker stays "in a call" with
        // a room nobody else joins, and every retry fails with
        // `AlreadyInCall` until the process restarts.
        let call_id_str = call.id.to_string();
        let channel_id_str = channel_id.to_string();
        if let Err(e) = self
            .send_call_offer(peer_id, &call_id_str, ephemeral_public, call_type_str, &channel_id_str)
            .await
        {
            tracing::error!(error = %e, call_id = %call.id, "Call offer send failed — rolling back call");
            call_worker
                .disconnect_local(CallEndReason::Error(format!("call offer not sent: {e}")))
                .await;
            return Err(e);
        }

        tracing::info!(
            peer_id = %peer_id,
            call_id = %call.id,
            "Call offer sent, storing pending state"
        );

        // Ringing from here on: register it so `end_active_call` can cancel
        // it (hangup + leave) before it connects. Registered before the
        // pending state is stored, so it precedes any answer's mark-connected
        // on the worker's queue.
        call_worker.orch_register_outgoing(
            CallHandle::from(call_id_str.as_str()),
            OrchestratedCallKind::DmStandard,
            *peer_id,
            String::new(),
            is_video,
        );

        // Store pending call state (ephemeral key kept until answer).
        self.set_pending_dm_call(PendingDmCall {
            call_id: call.id,
            channel_id,
            ephemeral_key,
            peer_id: *peer_id,
            call_type: call_type_str.to_string(),
            is_video,
        });

        // Signal's caller ring timeout: unanswered after RING_TIMEOUT, end it
        // exactly as a cancel does, so the callee stops ringing too. The
        // pending state is taken by the answer (`complete_dm_call`) and by any
        // end (`end_dm_call`), so still holding this call's means it rang out.
        let signaling = self.clone();
        let worker = call_worker.clone();
        let call_id = call.id;
        tokio::spawn(async move {
            tokio::time::sleep(RING_TIMEOUT).await;
            if signaling.pending_dm_call.lock().as_ref().map(|p| p.call_id) != Some(call_id) {
                return;
            }
            tracing::info!(call_id = %call_id, "Outgoing call unanswered — ending it");
            if let Err(e) = signaling.end_dm_call(&worker, CallEndReason::NoAnswer).await {
                tracing::debug!(error = %e, "ring timeout: call already gone");
            }
        });

        Ok(call.id)
    }

    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Consumes the pending call state, derives the shared secret via Signal service,
    /// updates LiveKit encryption, and returns verification data for the UI.
    #[cfg(feature = "media")]
    pub async fn complete_dm_call(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &UserId,
        signal_service: &SignalService,
        call_worker: &CallWorkerHandle,
    ) -> Result<CallSetupResult, CallSignalingError> {
        // Take pending call state
        let pending = self.take_pending_dm_call()
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Verify call ID matches
        if pending.call_id.to_string() != call_id {
            let expected = pending.call_id.to_string();
            // Put it back since this wasn't for us
            self.set_pending_dm_call(pending);
            return Err(CallSignalingError::CallIdMismatch {
                expected,
                actual: call_id.to_string(),
            });
        }

        tracing::info!(
            call_id = %call_id,
            peer_id = %peer_id,
            "Received call answer, deriving call key"
        );

        // Capture call view-data before `pending` is dropped (zeroizes the key).
        let is_video = pending.is_video;

        // Delegate all crypto to the Signal service (ECDH + identity lookup + HKDF)
        let priv_key = pending.ephemeral_key.private_key_bytes();
        let call_key = signal_service.actor
            .complete_call_key_exchange(
                priv_key,
                peer_ephemeral_pk,
                *peer_id,
                call_id.to_string(),
                true, // we_are_caller
            )
            .await
            .map_err(|e| CallSignalingError::KeyDerivationFailed(e.to_string()))?;

        // Ephemeral key dropped here (zeroized)
        drop(pending);
        tracing::debug!("Ephemeral private key zeroized");

        tracing::info!(
            call_id = %call_id,
            "Call key derived successfully, updating LiveKit encryption"
        );

        // Update LiveKit encryption key
        let key_installed = call_worker
            .set_encryption_key(call_key.to_vec())
            .await;

        // ponytail: the DM caller's auto-publish was deferred at initiate time
        // (media would otherwise have leaked under the all-zero placeholder
        // key). Now that the real ECDH key is installed at LiveKit index 0,
        // this is the first safe moment to publish — do it exactly once here.
        // Guard on install success: if the re-root failed the room is still on
        // the placeholder key, so we must not publish.
        if key_installed {
            call_worker.publish_microphone(None).await.ok();
            if is_video {
                call_worker.publish_camera(None).await.ok();
            }
        } else {
            tracing::error!(
                call_id = %call_id,
                "Real call key install failed — not publishing media to avoid placeholder-key leak"
            );
        }

        // Promote the ringing outgoing call (registered in `start_dm_call`) to
        // the orchestrator's active slot. The callee registers its slot via
        // `accept_presented_incoming_call`; this is the caller's equivalent.
        call_worker.orch_mark_connected(
            crate::services::call_worker::CallHandle::from(call_id),
            crate::services::call_worker::OrchestratedCallKind::DmStandard,
            *peer_id,
            is_video,
        );

        // Derive verification emojis
        let verification = CallVerification::derive_verification_emojis(&call_key, call_id);
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);

        let call_id_parsed = call_id.parse::<CallId>()
            .map_err(|_| CallSignalingError::KeyDerivationFailed("Invalid call ID".to_string()))?;

        Ok(CallSetupResult {
            call_id: call_id_parsed,
            channel_id: ChannelId::new(), // channel_id was consumed with pending
            verification_emojis: [e1, e2, e3, e4],
            is_video,
            // The call connects now — the answer just arrived. Counting from
            // the server's dial time ran the timer through the ring (and
            // through any server/device clock skew).
            started_at_ns: crate::util::time::TimestampNanos::now().as_u64(),
        })
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, send answer, join call.
    ///
    /// Performs the callee's side of the ephemeral key exchange. Returns
    /// verification data for the UI.
    #[cfg(feature = "media")]
    pub async fn accept_dm_call(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &UserId,
        signal_service: &SignalService,
        call_worker: &CallWorkerHandle,
        user_id: UserId,
    ) -> Result<CallSetupResult, CallSignalingError> {
        // Generate our ephemeral keypair
        let our_ephemeral = EphemeralCallKey::generate();
        let our_ephemeral_pk = our_ephemeral.public_key_bytes();

        // Delegate all crypto to the Signal service (ECDH + identity lookup + HKDF)
        let priv_key = our_ephemeral.private_key_bytes();
        let encryption_key = signal_service.actor
            .complete_call_key_exchange(
                priv_key,
                caller_ephemeral_pk,
                *caller_id,
                call_id.to_string(),
                false, // we_are_caller = false (we're the callee)
            )
            .await
            .map_err(|e| CallSignalingError::KeyDerivationFailed(e.to_string()))?;

        tracing::info!(
            call_id = %call_id,
            caller_id = %caller_id,
            "Derived call key, sending answer"
        );

        // Send answer with our ephemeral public key
        self.send_call_answer(caller_id, call_id, our_ephemeral_pk).await?;

        // Ephemeral key dropped here (zeroized)
        drop(our_ephemeral);
        tracing::debug!("Ephemeral private key zeroized");

        // Parse call ID
        let call_id_parsed = call_id.parse::<CallId>()
            .map_err(|_| CallSignalingError::KeyDerivationFailed("Invalid call ID".to_string()))?;

        // Join the call with derived key
        let call = call_worker
            .join_call(call_id_parsed, &encryption_key, user_id)
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        // Derive verification emojis
        let verification = CallVerification::derive_verification_emojis(
            &encryption_key,
            &call.id.to_string(),
        );
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);

        Ok(CallSetupResult {
            call_id: call.id,
            // Already the DM with the caller: `join_call` rewrites the
            // caller-scoped id (= us) — see `joined_channel_id`.
            channel_id: call.channel_id,
            verification_emojis: [e1, e2, e3, e4],
            is_video: false, // caller provides this info via the offer
            // Connected as of the join above; the server's `started_at` is the
            // caller's dial time, which would count the ring.
            started_at_ns: crate::util::time::TimestampNanos::now().as_u64(),
        })
    }

    // =========================================================================
    // Anonymous Call Orchestration
    // =========================================================================

    /// Claim a call-room token from the pre-fetched pool (already marked used).
    ///
    /// Never requests tokens: an authenticated request right before the
    /// anonymous room create/join that spends it links the two in time
    /// (NDSS'21 §VI-F). An empty pool fails the call with `NoTokens`; the
    /// refill loop tops it up on its own schedule.
    fn acquire_call_room_token(
        &self,
    ) -> Result<crate::mailbox::CallRoomToken, CallSignalingError> {
        self.anon_dm_service
            .token_manager
            .claim_prefetched_call_room_token()
            .map_err(|e| match e {
                crate::error::MailboxError::NoTokensAvailable => CallSignalingError::NoTokens,
                e => CallSignalingError::AnonCall(e.to_string()),
            })
    }

    /// Derive verification emojis from an anonymous call's encryption key and room name.
    #[cfg(feature = "media")]
    fn anon_verification_emojis(encryption_key: &[u8], room_name: &str) -> [String; 4] {
        let mut key_arr = [0u8; 32];
        if encryption_key.len() >= 32 {
            key_arr.copy_from_slice(&encryption_key[..32]);
        }
        let verification = CallVerification::derive_verification_emojis(&key_arr, room_name);
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);
        [e1, e2, e3, e4]
    }

    /// Whether `user_id` is the logged-in user — i.e. a message from one of our
    /// own devices.
    pub fn is_current_user(&self, user_id: &UserId) -> bool {
        self.channel_service.current_user_id().is_ok_and(|me| me == *user_id)
    }

    /// Drop the pending state of the anon call `sender` is ringing us with in
    /// `room_name`, returning whether there was one — i.e. whether their
    /// hangup calls off a ring of ours.
    pub fn take_incoming_anon_call(&self, room_name: &str, sender: UserId) -> bool {
        self.anon_dm_service.take_incoming_anon_call(room_name, sender).is_some()
    }

    /// Resolve the display name *we* locally know for an anon-DM peer, from the
    /// conversation's profile exchange. Preferred over the caller-supplied
    /// `caller_display_name` in an incoming offer: it's reliable (synced via the
    /// anon-DM handshake) and spoof-resistant — a caller can't pick the name
    /// shown on the callee's incoming-call screen. Returns `None` if unknown.
    pub fn anon_dm_peer_name(&self, conversation_id: &AnonDmId) -> Option<String> {
        self.anon_dm_service
            .get_conversation(conversation_id)
            .ok()
            .flatten()
            .and_then(|c| c.peer_display_name)
            .filter(|n| !n.is_empty())
    }

    /// Start an anonymous DM call: acquire token, send offer, store pending state.
    ///
    /// Returns an `AnonCallSetupResult` with the room name for UI display.
    /// The call is not yet connected -- that happens when the peer accepts
    /// via `handle_anon_call_answer()`.
    #[cfg(feature = "media")]
    pub async fn start_anon_dm_call(
        &self,
        anon_dm_id: &AnonDmId,
        is_video: bool,
        display_name: &str,
        call_worker: &CallWorkerHandle,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, CallSignalingError> {
        let token = self.acquire_call_room_token()?;

        // Send call offer via anon DM channel
        let offer_result = self.anon_dm_service
            .send_call_offer(anon_dm_id, &token, is_video, Some(display_name))
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send offer: {}", e)))?;

        tracing::info!(
            room_name = %offer_result.room_name,
            "Sent anonymous DM call offer"
        );

        // Store pending call state
        self.anon_dm_service.store_pending_anon_call(
            offer_result.room_name.clone(),
            crate::services::anon_dm::PendingAnonCall {
                call_room_id: offer_result.call_room_id,
                room_name: offer_result.room_name.clone(),
                conversation_id: *anon_dm_id,
                token,
                is_video_call: is_video,
                is_outgoing: true,
                display_name: display_name.to_string(),
                encryption_key: offer_result.call_encryption_key.clone(),
                sender_id: None,
                started_at: None,
            },
        );
        self.ring_anon_call(call_worker, &offer_result.room_name, OrchestratedCallKind::AnonDm, is_video);

        Ok(offer_result)
    }

    /// Start an anonymous group call: acquire token, send offer to all members, store pending state.
    #[cfg(feature = "media")]
    pub async fn start_anon_group_call(
        &self,
        group_id: &AnonGroupId,
        is_video: bool,
        display_name: &str,
        call_worker: &CallWorkerHandle,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, CallSignalingError> {
        let token = self.acquire_call_room_token()?;

        // Send call offer to all group members
        let offer_result = self.anon_group_service
            .send_call_offer(group_id, &token, is_video, Some(display_name))
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send group offer: {}", e)))?;

        tracing::info!(
            room_name = %offer_result.room_name,
            "Sent anonymous group call offer"
        );

        // Store pending call state (reuse anon DM pending call infrastructure)
        let conversation_id = crate::domain::anon_dm::AnonDmId::try_from_slice(
            group_id.as_bytes()
        ).expect("AnonGroupId is 16 bytes like AnonDmId");

        self.anon_dm_service.store_pending_anon_call(
            offer_result.room_name.clone(),
            crate::services::anon_dm::PendingAnonCall {
                call_room_id: offer_result.call_room_id,
                room_name: offer_result.room_name.clone(),
                conversation_id,
                token,
                is_video_call: is_video,
                is_outgoing: true,
                display_name: display_name.to_string(),
                encryption_key: offer_result.call_encryption_key.clone(),
                sender_id: None,
                started_at: None,
            },
        );
        self.ring_anon_call(call_worker, &offer_result.room_name, OrchestratedCallKind::AnonGroup, is_video);

        Ok(offer_result)
    }

    /// Our anon call is ringing: register it, so `end_active_call` can cancel
    /// it before anyone answers, and ring it out after [`RING_TIMEOUT`] —
    /// both through `end_anon_call`, so the callee stops ringing too.
    ///
    /// The peer is nil: anon peers have no `UserId` (as on connect, see
    /// `spawn_anon_call_answer_task`). The pending call is the ringing
    /// state — an answer re-stores it as started, and any end takes it — so
    /// the timeout ends the call only if it is still dialed and unanswered.
    #[cfg(feature = "media")]
    fn ring_anon_call(
        &self,
        call_worker: &CallWorkerHandle,
        room_name: &str,
        kind: OrchestratedCallKind,
        is_video: bool,
    ) {
        call_worker.orch_register_outgoing(
            CallHandle::from(room_name),
            kind,
            UserId(uuid::Uuid::nil()),
            String::new(),
            is_video,
        );
        let signaling = self.clone();
        let worker = call_worker.clone();
        let room_name = room_name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(RING_TIMEOUT).await;
            let Some(pending) = signaling.anon_dm_service.take_ringing_anon_call(Some(&room_name)) else {
                return;
            };
            tracing::info!(room_name = %room_name, "Outgoing anon call unanswered — ending it");
            signaling.finish_anon_call(&worker, Some(pending), room_name, 0, CallEndReason::NoAnswer).await;
        });
    }

    /// Accept an incoming anonymous call: acquire token, send accept, join room, connect LiveKit.
    ///
    /// Returns an `AnonCallSetupResult` with everything the UI needs.
    #[cfg(feature = "media")]
    pub async fn accept_anon_call(
        &self,
        room_name: &str,
        call_worker: &CallWorkerHandle,
    ) -> Result<AnonCallSetupResult, CallSignalingError> {
        // Take pending call state
        let pending = self.anon_dm_service.take_pending_anon_call(room_name)
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Acquire a fresh token for joining
        let token = self.acquire_call_room_token()
            .map_err(|e| {
                // Put pending call back so user can retry
                self.anon_dm_service.store_pending_anon_call(room_name.to_string(), pending.clone());
                e
            })?;

        // Use our own display name
        let my_display_name = self.anon_dm_service
            .get_my_display_name()
            .unwrap_or_else(|| "Anonymous".to_string());

        // Determine if this is a group call (no anon DM conversation for this ID)
        let is_group_call = self.anon_dm_service
            .get_conversation(&pending.conversation_id)
            .ok().flatten().is_none();

        // Send accept answer. The returned ns timestamp is wired into the
        // peer's `AnonCallAnswer` envelope and becomes the shared call-start
        // anchor for the duration timer on both sides.
        let answer_timestamp_ns = if is_group_call {
            let group_id = AnonGroupId::try_from_slice(
                pending.conversation_id.as_bytes()
            ).ok_or_else(|| CallSignalingError::AnonCall("Invalid group ID".into()))?;

            self.anon_group_service
                .send_call_answer(&group_id, &pending.call_room_id, true, Some(&token), Some(&my_display_name))
                .await
                .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send group accept: {}", e)))?
        } else {
            self.anon_dm_service
                .send_call_answer(
                    &pending.conversation_id,
                    &pending.call_room_id,
                    true,
                    Some(&token),
                    None,
                    Some(&my_display_name),
                )
                .await
                .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send accept: {}", e)))?
        };

        tracing::info!(room_name = %room_name, "Anonymous call accept sent, joining LiveKit room");

        // Join the anonymous call room to get LiveKit credentials
        let room_info = self.anon_dm_service
            .join_anon_call_room(room_name, &token, &my_display_name, pending.is_video_call)
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to join room: {}", e)))?;

        // Connect to LiveKit
        let call_id = CallId::new();
        let channel_id = ChannelId::new();

        call_worker
            .connect_with_token(
                &room_info.livekit_token,
                &room_info.livekit_url,
                pending.encryption_key.clone(),
                room_name,
                call_id,
                channel_id,
                pending.is_video_call,
            )
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        tracing::info!(room_name = %room_name, "Connected to LiveKit for anonymous call");

        let verification_emojis = Self::anon_verification_emojis(&pending.encryption_key, room_name);

        let peer_avatar = self
            .anon_dm_service
            .get_conversation(&pending.conversation_id)
            .ok()
            .flatten()
            .and_then(|c| c.peer_avatar);

        let result = AnonCallSetupResult {
            room_name: room_name.to_string(),
            channel_id,
            encryption_key: zeroize::Zeroizing::new(pending.encryption_key.clone()),
            verification_emojis,
            is_video: pending.is_video_call,
            peer_display_name: pending.display_name.clone(),
            started_at_ns: answer_timestamp_ns,
            peer_avatar,
        };

        // Re-store pending call so end_anon_call can find it for hangup/history
        let mut active_pending = pending;
        active_pending.started_at = Some(chrono::Utc::now());
        self.anon_dm_service.store_pending_anon_call(room_name.to_string(), active_pending);

        Ok(result)
    }

    /// Handle an anonymous call answer (caller side): acquire token, join room, connect LiveKit.
    ///
    /// Group anon calls fan out answers to every participant, so this may be
    /// invoked repeatedly for the same room. Returns `AlreadyInCall` for those
    /// echoes so the UI can distinguish them from a real `Declined`.
    #[cfg(feature = "media")]
    pub async fn handle_anon_call_answer(
        &self,
        call_room_id: &[u8],
        accepted: bool,
        callee_display_name: &str,
        answer_timestamp_ns: u64,
        call_worker: &CallWorkerHandle,
    ) -> Result<AnonCallAnswerOutcome, CallSignalingError> {
        // Skip if already in a call — this handles the callee's own echo coming
        // back from the group mailbox, or another participant's answer in a
        // multi-party group call. Distinct from `Declined` so the UI can ignore
        // these silently instead of tearing down the active call.
        if call_worker.is_in_call().await {
            tracing::debug!("Already in a call, ignoring duplicate answer event");
            return Ok(AnonCallAnswerOutcome::AlreadyInCall);
        }

        // Reconstruct room name from call_room_id. A wrong-length id can't map
        // to any pending call — surface it instead of silently degrading to a
        // nil-UUID room name that always misses (misleading NoPendingCall).
        let id_bytes: [u8; 16] = call_room_id.try_into().map_err(|_| {
            CallSignalingError::AnonCall(format!(
                "invalid call_room_id length: {} (expected 16)",
                call_room_id.len()
            ))
        })?;
        let room_name = format!("anon-{}", uuid::Uuid::from_bytes(id_bytes));

        let pending = self.anon_dm_service.take_pending_anon_call(&room_name)
            .ok_or(CallSignalingError::NoPendingCall)?;

        if !accepted {
            tracing::info!(room_name = %room_name, "Anonymous call declined by peer");
            return Ok(AnonCallAnswerOutcome::Declined);
        }

        tracing::info!(room_name = %room_name, "Anonymous call accepted, connecting to LiveKit");

        // Mark call as started for duration tracking
        self.anon_dm_service.mark_anon_call_started(&pending.room_name);

        // Acquire a fresh token for joining the call room
        let join_token = self.acquire_call_room_token()?;

        // Join the anonymous call room to get LiveKit credentials
        let room_info = self.anon_dm_service
            .join_anon_call_room(
                &pending.room_name,
                &join_token,
                &pending.display_name,
                pending.is_video_call,
            )
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to join room: {}", e)))?;

        // Connect to LiveKit
        let call_id = CallId::new();
        let channel_id = ChannelId::new();

        call_worker
            .connect_with_token(
                &room_info.livekit_token,
                &room_info.livekit_url,
                pending.encryption_key.clone(),
                &pending.room_name,
                call_id,
                channel_id,
                pending.is_video_call,
            )
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        tracing::info!(room_name = %room_name, "Connected to LiveKit for anonymous call (caller side)");

        let verification_emojis = Self::anon_verification_emojis(&pending.encryption_key, &pending.room_name);

        let peer_avatar = self
            .anon_dm_service
            .get_conversation(&pending.conversation_id)
            .ok()
            .flatten()
            .and_then(|c| c.peer_avatar);

        let result = AnonCallSetupResult {
            room_name: pending.room_name.clone(),
            channel_id,
            encryption_key: zeroize::Zeroizing::new(pending.encryption_key.clone()),
            verification_emojis,
            is_video: pending.is_video_call,
            peer_display_name: callee_display_name.to_string(),
            started_at_ns: answer_timestamp_ns,
            peer_avatar,
        };

        // Re-store pending call so end_anon_call can find it for hangup/history
        let mut active_pending = pending;
        active_pending.started_at = Some(chrono::Utc::now());
        self.anon_dm_service.store_pending_anon_call(room_name.clone(), active_pending);

        Ok(AnonCallAnswerOutcome::Connected(result))
    }

    /// Decline an incoming anonymous call: send the decline answer. The history
    /// row and bubble follow the orchestrator's `IncomingWithdrawn { Declined }`
    /// (`call_history_recorder`).
    pub async fn decline_anon_call(
        &self,
        room_name: &str,
    ) -> Result<(), CallSignalingError> {
        let pending = self.anon_dm_service.take_pending_anon_call(room_name)
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Determine if this is a group call
        let is_group_call = self.anon_dm_service
            .get_conversation(&pending.conversation_id)
            .ok().flatten().is_none();

        if is_group_call {
            let group_id = AnonGroupId::try_from_slice(
                pending.conversation_id.as_bytes()
            ).ok_or_else(|| CallSignalingError::AnonCall("Invalid group ID".into()))?;

            self.anon_group_service
                .send_call_answer(&group_id, &pending.call_room_id, false, None, Some(&pending.display_name))
                .await
                .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send group decline: {}", e)))?;
        } else {
            self.anon_dm_service
                .send_call_answer(
                    &pending.conversation_id,
                    &pending.call_room_id,
                    false,
                    None,
                    None,
                    Some(&pending.display_name),
                )
                .await
                .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send decline: {}", e)))?;
        }

        tracing::info!(room_name = %room_name, "Anonymous call declined");

        Ok(())
    }

    // ================================================================
    // End-call orchestration (hangup + history + leave)
    // ================================================================

    /// End our DM call, connected or still ringing: send hangup, leave.
    ///
    /// `call_worker` must be provided since it's behind a OnceLock in the client.
    /// `reason` is what the leave's `ActiveEnded` reports. The Call bubble is
    /// filed by `call_history_recorder` on that event — the one place every
    /// end of a DM call reaches.
    #[cfg(feature = "media")]
    pub async fn end_dm_call(
        &self,
        call_worker: &CallWorkerHandle,
        reason: CallEndReason,
    ) -> Result<(), CallSignalingError> {
        // A call ended while ringing is over: an answer that lands after this
        // must not complete it (`complete_dm_call` would mark a dead call
        // connected), nor may the ring timeout end it again.
        self.take_pending_dm_call();

        let call = call_worker.active_call().await
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Send hangup notification to peer
        if let Err(e) = self.send_call_hangup(
            &call.channel_id, &call.id.to_string(), CallHangupReason::Normal,
        ).await {
            tracing::error!(error = %e, "Failed to send hangup notification");
        }

        // Leave the call (orchestrator cleanup is now atomic inside the worker)
        if let Err(e) = call_worker.leave_call(reason).await {
            tracing::error!(error = %e, "Failed to leave call");
        }

        Ok(())
    }

    /// End an active group/channel call: store history, then leave the room.
    ///
    /// No DM hangup is sent — a channel call has many members, not a single
    /// peer. Departure is signalled by the LiveKit `ParticipantDisconnected`
    /// event when we leave the room; the initiator rekeys for the remaining
    /// members via the participant-departed path.
    #[cfg(feature = "media")]
    pub async fn end_group_call(
        &self,
        call_worker: &CallWorkerHandle,
        duration_secs: i64,
    ) -> Result<(), CallSignalingError> {
        let call = call_worker.active_call().await
            .ok_or(CallSignalingError::NoPendingCall)?;

        let current_user = self.channel_service.current_user_id()
            .unwrap_or_else(|_| UserId::from_uuid(uuid::Uuid::nil()));
        let is_outgoing = call.initiator_id == current_user;

        self.store_group_call_history(
            &call.channel_id, &call.id.to_string(),
            &call.initiator_id, &call.initiator_id.to_string(),
            is_outgoing, duration_secs, false,
        );

        // Leave the room — this is what tears down media + audio. Done last so
        // history is recorded even if the worker shut down early.
        if let Err(e) = call_worker.leave_call(CallEndReason::LocalHangup).await {
            tracing::error!(error = %e, "Failed to leave group call");
        }

        Ok(())
    }

    /// End an active anonymous call: send hangup, store history, leave.
    ///
    /// Handles both anonymous DM and anonymous group calls.
    /// Always disconnects from the LiveKit room, even if hangup signaling fails.
    #[cfg(feature = "media")]
    pub async fn end_anon_call(
        &self,
        call_worker: &CallWorkerHandle,
        duration_secs: i64,
    ) -> Result<Option<crate::domain::anon_dm::AnonDmId>, CallSignalingError> {
        // Get room name from active call state (not from Call.id which is a random UUID)
        if let Some(room_name) = call_worker.get_room_name().await {
            // Take the pending call info (re-stored by accept/answer flow)
            let pending = self.anon_dm_service.take_pending_anon_call(&room_name);
            return Ok(self
                .finish_anon_call(call_worker, pending, room_name, duration_secs, CallEndReason::LocalHangup)
                .await);
        }
        // No room: our call is still ringing — the caller joins the room
        // only once it is answered. Cancel it.
        if let Some(pending) = self.anon_dm_service.take_ringing_anon_call(None) {
            let room_name = pending.room_name.clone();
            return Ok(self
                .finish_anon_call(call_worker, Some(pending), room_name, 0, CallEndReason::LocalHangup)
                .await);
        }
        tracing::warn!("end_anon_call: no active or ringing anon call, forcing disconnect");
        call_worker.disconnect_local(CallEndReason::LocalHangup).await;
        Ok(None)
    }

    /// Send the anon hangup and end `room_name`'s call with `reason`: leave
    /// the room if we are in it, else withdraw our ringing call from the
    /// orchestrator. Either way one `ActiveEnded` closes the history row,
    /// files a 1:1 call's bubble, and closes the UI's call view.
    #[cfg(feature = "media")]
    async fn finish_anon_call(
        &self,
        call_worker: &CallWorkerHandle,
        pending: Option<crate::services::anon_dm::PendingAnonCall>,
        room_name: String,
        duration_secs: i64,
        reason: CallEndReason,
    ) -> Option<crate::domain::anon_dm::AnonDmId> {
        use crate::proto::content_v1::anon_call_hangup::Reason as HangupReason;
        let hangup_reason = if reason == CallEndReason::NoAnswer {
            HangupReason::Timeout
        } else {
            HangupReason::Normal
        };

        // Send hangup and store history if we have the call context
        let conversation_id = if let Some(ref pending) = pending {
            let conversation_id = pending.conversation_id;

            // Determine if this is a group call
            let is_group_call = self.anon_dm_service
                .get_conversation(&conversation_id)
                .ok().flatten().is_none();

            // Send hangup to the correct channel (DM or group)
            if is_group_call {
                if let Some(group_id) = AnonGroupId::try_from_slice(conversation_id.as_bytes()) {
                    if let Err(e) = self.anon_group_service.send_call_hangup(
                        &group_id, &pending.call_room_id, hangup_reason,
                    ).await {
                        tracing::error!(error = %e, "Failed to send group call hangup");
                    }
                }
            } else {
                if let Err(e) = self.anon_dm_service.send_call_hangup(
                    &conversation_id, &pending.call_room_id, hangup_reason,
                ).await {
                    tracing::error!(error = %e, "Failed to send anonymous call hangup");
                }
            }

            // A 1:1 anon call's Call bubble is filed by `call_history_recorder`
            // on the `ActiveEnded` below, as for a friend DM; only a group call
            // still files its text record here.
            if is_group_call {
                // Store call history as plain text (store_text_message wraps in
                // MessageContent::Text without a JSON envelope, so the bubble renders
                // correctly after reload via Message::as_text()).
                let duration_str = if duration_secs > 0 {
                    format!("{}:{:02}", duration_secs / 60, duration_secs % 60)
                } else {
                    "0:00".to_string()
                };
                let label = if pending.is_outgoing { "Outgoing call" } else { "Incoming call" };
                let text = format!("\u{1F4DE} {} \u{00B7} {}", label, duration_str);
                let sent_at = crate::util::time::TimestampNanos::now().as_u64();
                let user_id = self.channel_service.current_user_id()
                    .unwrap_or_else(|_| UserId::from_uuid(uuid::Uuid::nil()));

                match self.anon_dm_service.store_text_message(&conversation_id, &user_id, &text, sent_at) {
                    Ok(msg_id) => {
                        // Tell the UI in real time so the call-history bubble
                        // shows without waiting for the user to re-enter the
                        // conversation. The send path optimistically inserts
                        // via `send_message.rs` and skips this emit; here the
                        // message originates inside the signaling actor with
                        // no UI optimism, so we have to drive the model
                        // update ourselves.
                        use crate::domain::message::{ConversationType, Message, MessageContent};
                        use crate::domain::ids::{ChannelId, MessageId};
                        use crate::inbox::handlers::DomainEvent;
                        let channel_id: ChannelId = conversation_id.into();
                        let msg_uuid = msg_id.as_str().parse::<uuid::Uuid>()
                            .map(MessageId::from_uuid)
                            .unwrap_or_else(|_| MessageId::new());
                        let now = chrono::Utc::now();
                        let domain_msg = Message {
                            id: msg_uuid,
                            conversation_id: channel_id,
                            conversation_type: ConversationType::AnonDm,
                            sender_id: Some(user_id),
                            device_id: None,
                            content: MessageContent::Text(text),
                            encrypted_content: None,
                            thread_id: None,
                            reply_to: None,
                            reply_to_author_id: None,
                            reply_to_text: None,
                            reply_to_thumbnail: None,
                            reply_to_thumbnail_mime: None,
                            attachments: vec![],
                            is_pinned: false,
                            is_edited: false,
                            is_deleted: false,
                            is_read: true,
                            sent_at,
                            created_at: now,
                            updated_at: now,
                            edited_at: None,
                            reaction_counts: vec![],
                            current_user_reactions: vec![],
                            mentioned_integrations: vec![],
                            targets_ai_bot: false,
                            delivery_status: String::new(),
                        };
                        self.message_service.event_bus.emit_domain(
                            DomainEvent::MessageReceived { message: domain_msg },
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to store anon call history on hangup");
                    }
                }
            }

            Some(conversation_id)
        } else {
            tracing::warn!(room_name = %room_name, "No pending call context for end — hangup signal skipped");
            None
        };

        // Leave the call (orchestrator cleanup is now atomic inside the
        // worker); a call that never got a room is only the orchestrator's.
        if call_worker.get_room_name().await.as_deref() == Some(room_name.as_str()) {
            if let Err(e) = call_worker.leave_call(reason).await {
                tracing::debug!(error = %e, "leave_call after anon hangup");
            }
        } else {
            call_worker.orch_mark_ended(CallHandle::from(room_name), reason);
        }

        conversation_id
    }

    /// Handle remote hangup: disconnect locally. The Call bubble is filed by
    /// `call_history_recorder` on the resulting `ActiveEnded`.
    #[cfg(feature = "media")]
    pub async fn handle_remote_hangup(
        &self,
        call_worker: &CallWorkerHandle,
        expected_call_id: Option<&str>,
    ) -> Result<(), CallSignalingError> {
        let active = call_worker.active_call().await;
        // A named hangup for a call we are not in changes nothing: with no
        // active call there is nothing of ours to end (a ringing one is
        // withdrawn by the orchestrator, not here).
        if active.is_none() && expected_call_id.is_some() {
            return Ok(());
        }
        if let (Some(call), Some(expected)) = (active, expected_call_id) {
            // Ignore hangups for a call we are not in. Moved here so the UI
            // doesn't need to reach into call state to gate the event.
            if call.id.to_string() != expected {
                tracing::debug!(
                    expected,
                    active = %call.id,
                    "Ignoring remote hangup for non-active call"
                );
                return Ok(());
            }
        }

        call_worker.disconnect_local(CallEndReason::RemoteHangup).await;
        Ok(())
    }

    /// Store a group/channel call history message. (A DM call's bubble is
    /// filed by `call_history_recorder`.)
    #[cfg(feature = "media")]
    fn store_group_call_history(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        initiator_id: &UserId,
        initiator_name: &str,
        _is_outgoing: bool,
        duration_secs: i64,
        is_missed: bool,
    ) {
        use crate::domain::message::*;

        let call_type = if is_missed { CallMessageType::Missed } else { CallMessageType::Ended };
        let duration = if duration_secs > 0 {
            Some(chrono::Duration::seconds(duration_secs))
        } else {
            None
        };

        let call_msg = CallMessage {
            call_id: call_id.to_string(),
            call_type,
            initiator_id: *initiator_id,
            initiator_name: initiator_name.to_string(),
            started_at: None,
            ended_at: Some(chrono::Utc::now()),
            duration,
            participant_count: 2,
        };

        let message = Message {
            id: crate::domain::ids::MessageId::new(),
            conversation_id: *channel_id,
            // Derive from the channel row, so a hardcoded type can't
            // mislabel the call's conversation.
            conversation_type: self.message_service.conversation_type(channel_id),
            sender_id: Some(*initiator_id),
            device_id: None,
            content: MessageContent::Call(call_msg),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: vec![],
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at: crate::util::time::TimestampNanos::now().as_u64(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            delivery_status: String::new(),
        };

        if let Err(e) = self.message_service.store_message_local(&message) {
            tracing::error!(error = %e, "Failed to save call history");
        }
    }
}

/// Recipients for a rotated group call key.
///
/// The live LiveKit room roster (`room_identities`, each entry a peer's
/// `user_id` — the server mints call tokens with `SetIdentity(user_id)`)
/// intersected with channel membership, minus ourselves. The roster decides
/// *who still needs the key*; membership decides *who is entitled to it*, so a
/// room identity that is not a channel member is dropped rather than sent to.
fn rotation_recipients(
    members: &[crate::domain::channel::Membership],
    room_identities: &[String],
    current_user: &UserId,
) -> Vec<UserId> {
    let in_room: std::collections::HashSet<&str> =
        room_identities.iter().map(|s| s.as_str()).collect();
    members
        .iter()
        .filter(|m| &m.user_id != current_user)
        .filter(|m| in_room.contains(m.user_id.to_string().as_str()))
        .map(|m| m.user_id)
        .collect()
}

#[cfg(test)]
mod rotation_recipient_tests {
    use super::*;
    use crate::domain::channel::{MemberRole, Membership, NotificationPreference};

    fn member(channel_id: &ChannelId, user_id: &UserId) -> Membership {
        Membership {
            channel_id: *channel_id,
            user_id: user_id.clone(),
            role: MemberRole::Member,
            joined_at: chrono::Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::default(),
        }
    }

    /// A participant who left the LiveKit room is still a channel member.
    /// Rotation exists to lock them out, so they must NOT be a recipient.
    #[test]
    fn departed_member_does_not_receive_the_rotated_key() {
        let channel_id = ChannelId::new();
        let me = UserId::new();
        let stayed = UserId::new();
        let departed = UserId::new();

        let members = vec![
            member(&channel_id, &me),
            member(&channel_id, &stayed),
            member(&channel_id, &departed),
        ];
        // Roster sampled after the departure: only `stayed` is still in the room.
        let roster = vec![stayed.to_string()];

        let recipients = rotation_recipients(&members, &roster, &me);

        assert!(
            !recipients.contains(&departed),
            "rotated key was sent to the departed participant {departed} — rotation is a no-op"
        );
        assert_eq!(recipients, vec![stayed.clone()]);
    }

    /// Authorisation: a room identity with no channel membership is not
    /// entitled to the channel's call key, so it is dropped.
    #[test]
    fn room_identity_without_membership_is_not_sent_to() {
        let channel_id = ChannelId::new();
        let me = UserId::new();
        let stayed = UserId::new();
        let intruder = UserId::new();

        let members = vec![member(&channel_id, &me), member(&channel_id, &stayed)];
        let roster = vec![stayed.to_string(), intruder.to_string()];

        let recipients = rotation_recipients(&members, &roster, &me);

        assert_eq!(recipients, vec![stayed.clone()]);
        assert!(!recipients.contains(&intruder));
    }

    /// We rotate our own key locally; never DM it to ourselves.
    #[test]
    fn self_is_excluded_even_when_on_the_roster() {
        let channel_id = ChannelId::new();
        let me = UserId::new();
        let members = vec![member(&channel_id, &me)];
        let roster = vec![me.to_string()];

        assert!(rotation_recipients(&members, &roster, &me).is_empty());
    }
}
