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

#[cfg(feature = "desktop-media")]
use crate::crypto::call_key_provider::EphemeralCallKey;
use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
#[cfg(feature = "desktop-media")]
use crate::domain::call::{AnonCallSetupResult, CallSetupResult, CallType};
use crate::domain::call::PendingDmCall;
#[cfg(feature = "desktop-media")]
use crate::domain::ids::CallId;
use crate::domain::ids::{ChannelId, UserId};
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
#[cfg(feature = "desktop-media")]
use crate::services::call_verification::CallVerification;
#[cfg(feature = "desktop-media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::gateway::{
    build_call_key_offer, build_call_key_answer,
    build_call_hangup_content, build_group_call_key_content,
    CallHangupReason,
};
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
#[cfg(feature = "desktop-media")]
use crate::services::signal::SignalService;

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
}

/// Handles call signaling (key exchange, hangup) over the Signal Protocol.
#[derive(Clone)]
pub struct CallSignalingService {
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
    channel_service: Arc<ChannelService>,
    anon_dm_service: Arc<AnonDmService>,
    anon_group_service: Arc<AnonGroupService>,
    /// Pending outgoing DM call state (waiting for answer with peer's ephemeral key).
    pending_dm_call: Arc<Mutex<Option<PendingDmCall>>>,
}

impl CallSignalingService {
    pub fn new(
        message_service: Arc<MessageService>,
        mailbox_manager: Arc<MailboxManager>,
        channel_service: Arc<ChannelService>,
        anon_dm_service: Arc<AnonDmService>,
        anon_group_service: Arc<AnonGroupService>,
    ) -> Self {
        Self {
            message_service,
            mailbox_manager,
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

    /// Clear any pending outgoing DM call state.
    pub fn clear_pending_dm_call(&self) {
        *self.pending_dm_call.lock() = None;
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
        self.message_service
            .send_dm(peer_id, content, &self.mailbox_manager)
            .await?;
        tracing::info!(peer_id = %peer_id, call_id, "Call offer sent");
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
            .send_dm(peer_id, content, &self.mailbox_manager)
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
        let peer_id = self.channel_service
            .get_dm_peer_local(channel_id)?
            .ok_or_else(|| CallSignalingError::NoPeer(*channel_id))?;

        let content = build_call_hangup_content(call_id, reason);
        self.message_service
            .send_dm(&peer_id, content, &self.mailbox_manager)
            .await?;
        tracing::info!(peer_id = %peer_id, call_id, "Call hangup sent");
        Ok(())
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
        let members = self.channel_service.get_channel_members(channel_id, 100, 0).await?;
        let channel_id_str = channel_id.to_string();

        let mut sent_count = 0;
        for member in &members {
            if member.user_id == current_user {
                continue;
            }

            let content = build_group_call_key_content(call_id, key, epoch, &channel_id_str);
            if let Err(e) = self.message_service
                .send_dm(&member.user_id, content, &self.mailbox_manager)
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
    #[cfg(feature = "desktop-media")]
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

        // Send offer via sealed sender DM
        let call_id_str = call.id.to_string();
        let channel_id_str = channel_id.to_string();
        self.send_call_offer(peer_id, &call_id_str, ephemeral_public, call_type_str, &channel_id_str)
            .await?;

        tracing::info!(
            peer_id = %peer_id,
            call_id = %call.id,
            "Call offer sent, storing pending state"
        );

        // Store pending call state (ephemeral key kept until answer)
        self.set_pending_dm_call(PendingDmCall {
            call_id: call.id,
            channel_id,
            ephemeral_key,
            peer_id: *peer_id,
            call_type: call_type_str.to_string(),
            is_video,
        });

        Ok(call.id)
    }

    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Consumes the pending call state, derives the shared secret via Signal service,
    /// updates LiveKit encryption, and returns verification data for the UI.
    #[cfg(feature = "desktop-media")]
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
        call_worker
            .set_encryption_key(call_key.to_vec())
            .await;

        // Derive verification emojis
        let verification = CallVerification::derive_verification_emojis(&call_key, call_id);
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);

        let call_id_parsed = call_id.parse::<CallId>()
            .map_err(|_| CallSignalingError::KeyDerivationFailed("Invalid call ID".to_string()))?;

        Ok(CallSetupResult {
            call_id: call_id_parsed,
            channel_id: ChannelId::new(), // channel_id was consumed with pending
            verification_emojis: [e1, e2, e3, e4],
            is_video: false, // caller knows this from the pending state
        })
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, send answer, join call.
    ///
    /// Performs the callee's side of the ephemeral key exchange. Returns
    /// verification data for the UI.
    #[cfg(feature = "desktop-media")]
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
            channel_id: call.channel_id,
            verification_emojis: [e1, e2, e3, e4],
            is_video: false, // caller provides this info via the offer
        })
    }

    // =========================================================================
    // Anonymous Call Orchestration
    // =========================================================================

    /// Acquire a call room token, requesting more from the server if none available.
    ///
    /// Tries both anon DM and anon group token managers (they share the same pool).
    async fn acquire_call_room_token(
        &self,
    ) -> Result<crate::mailbox::CallRoomToken, CallSignalingError> {
        // Try to get an existing token
        if let Ok(Some(token)) = self.anon_dm_service.get_available_call_room_token() {
            return Ok(token);
        }

        // Request more tokens from server
        tracing::info!("No call room tokens available, requesting 5 more");
        self.anon_dm_service
            .request_call_room_tokens(5)
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to request tokens: {}", e)))?;

        // Try again
        self.anon_dm_service
            .get_available_call_room_token()
            .map_err(|e| CallSignalingError::AnonCall(e.to_string()))?
            .ok_or(CallSignalingError::NoTokens)
    }

    /// Derive verification emojis from an anonymous call's encryption key and room name.
    #[cfg(feature = "desktop-media")]
    fn anon_verification_emojis(encryption_key: &[u8], room_name: &str) -> [String; 4] {
        let mut key_arr = [0u8; 32];
        if encryption_key.len() >= 32 {
            key_arr.copy_from_slice(&encryption_key[..32]);
        }
        let verification = CallVerification::derive_verification_emojis(&key_arr, room_name);
        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&verification);
        [e1, e2, e3, e4]
    }

    /// Start an anonymous DM call: acquire token, send offer, store pending state.
    ///
    /// Returns an `AnonCallSetupResult` with the room name for UI display.
    /// The call is not yet connected -- that happens when the peer accepts
    /// via `handle_anon_call_answer()`.
    pub async fn start_anon_dm_call(
        &self,
        anon_dm_id: &AnonDmId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, CallSignalingError> {
        // Acquire token (request more if needed)
        let token = self.acquire_call_room_token().await?;

        // Send call offer via anon DM channel
        let offer_result = self.anon_dm_service
            .send_call_offer(anon_dm_id, &token, is_video, Some(display_name))
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send offer: {}", e)))?;

        tracing::info!(
            room_name = %offer_result.room_name,
            "Sent anonymous DM call offer"
        );

        // Mark token as used
        let _ = self.anon_dm_service.mark_call_room_token_used(&token.id);

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

        Ok(offer_result)
    }

    /// Start an anonymous group call: acquire token, send offer to all members, store pending state.
    pub async fn start_anon_group_call(
        &self,
        group_id: &AnonGroupId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, CallSignalingError> {
        // Acquire token (request more if needed)
        let token = self.acquire_call_room_token().await?;

        // Send call offer to all group members
        let offer_result = self.anon_group_service
            .send_call_offer(group_id, &token, is_video, Some(display_name))
            .await
            .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send group offer: {}", e)))?;

        tracing::info!(
            room_name = %offer_result.room_name,
            "Sent anonymous group call offer"
        );

        // Mark token as used
        let _ = self.anon_group_service.mark_call_room_token_used(&token.id);

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

        Ok(offer_result)
    }

    /// Accept an incoming anonymous call: acquire token, send accept, join room, connect LiveKit.
    ///
    /// Returns an `AnonCallSetupResult` with everything the UI needs.
    #[cfg(feature = "desktop-media")]
    pub async fn accept_anon_call(
        &self,
        room_name: &str,
        call_worker: &CallWorkerHandle,
    ) -> Result<AnonCallSetupResult, CallSignalingError> {
        // Take pending call state
        let pending = self.anon_dm_service.take_pending_anon_call(room_name)
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Acquire a fresh token for joining
        let token = self.acquire_call_room_token().await
            .map_err(|e| {
                // Put pending call back so user can retry
                self.anon_dm_service.store_pending_anon_call(room_name.to_string(), pending.clone());
                e
            })?;
        let _ = self.anon_dm_service.mark_call_room_token_used(&token.id);

        // Use our own display name
        let my_display_name = self.anon_dm_service
            .get_my_display_name()
            .unwrap_or_else(|| "Anonymous".to_string());

        // Determine if this is a group call (no anon DM conversation for this ID)
        let is_group_call = self.anon_dm_service
            .get_conversation(&pending.conversation_id)
            .ok().flatten().is_none();

        // Send accept answer
        if is_group_call {
            let group_id = AnonGroupId::try_from_slice(
                pending.conversation_id.as_bytes()
            ).ok_or_else(|| CallSignalingError::AnonCall("Invalid group ID".into()))?;

            self.anon_group_service
                .send_call_answer(&group_id, &pending.call_room_id, true, Some(&token), Some(&my_display_name))
                .await
                .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send group accept: {}", e)))?;
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
                .map_err(|e| CallSignalingError::AnonCall(format!("Failed to send accept: {}", e)))?;
        }

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
            )
            .await
            .map_err(|e| CallSignalingError::CallWorker(e.to_string()))?;

        tracing::info!(room_name = %room_name, "Connected to LiveKit for anonymous call");

        let verification_emojis = Self::anon_verification_emojis(&pending.encryption_key, room_name);

        let result = AnonCallSetupResult {
            room_name: room_name.to_string(),
            channel_id,
            encryption_key: pending.encryption_key.clone(),
            verification_emojis,
            is_video: pending.is_video_call,
            peer_display_name: pending.display_name.clone(),
        };

        // Re-store pending call so end_anon_call can find it for hangup/history
        let mut active_pending = pending;
        active_pending.started_at = Some(chrono::Utc::now());
        self.anon_dm_service.store_pending_anon_call(room_name.to_string(), active_pending);

        Ok(result)
    }

    /// Handle an anonymous call answer (caller side): acquire token, join room, connect LiveKit.
    ///
    /// If accepted, returns `Some(AnonCallSetupResult)`. If declined, stores call history and returns `None`.
    #[cfg(feature = "desktop-media")]
    pub async fn handle_anon_call_answer(
        &self,
        call_room_id: &[u8],
        accepted: bool,
        callee_display_name: &str,
        call_worker: &CallWorkerHandle,
    ) -> Result<Option<AnonCallSetupResult>, CallSignalingError> {
        // Skip if already in a call — this handles the callee's own echo coming
        // back from the group mailbox, or duplicate answer events.
        if call_worker.is_in_call().await {
            tracing::debug!("Already in a call, skipping duplicate answer event");
            return Ok(None);
        }

        // Reconstruct room name from call_room_id
        let room_name = format!(
            "anon-{}",
            uuid::Uuid::from_bytes(
                call_room_id.try_into().unwrap_or_default()
            )
        );

        let pending = self.anon_dm_service.take_pending_anon_call(&room_name)
            .ok_or(CallSignalingError::NoPendingCall)?;

        if !accepted {
            tracing::info!(room_name = %room_name, "Anonymous call declined by peer");

            // Store declined call history
            let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
            if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                let anonymous_user_id = UserId::new();
                self.store_anon_call_history_failed(
                    &channel_id,
                    &pending.room_name,
                    &anonymous_user_id,
                    callee_display_name,
                    pending.is_outgoing,
                );
            }

            return Ok(None);
        }

        tracing::info!(room_name = %room_name, "Anonymous call accepted, connecting to LiveKit");

        // Mark call as started for duration tracking
        self.anon_dm_service.mark_anon_call_started(&pending.room_name);

        // Acquire a fresh token for joining the call room
        let join_token = self.acquire_call_room_token().await?;
        let _ = self.anon_dm_service.mark_call_room_token_used(&join_token.id);

        // Join the anonymous call room to get LiveKit credentials
        let room_info = self.anon_dm_service
            .join_anon_call_room(
                &pending.room_name,
                &join_token,
                &pending.display_name,
                pending.is_video_call,
            )
            .await
            .map_err(|e| {
                // Store failed call history
                let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                    let anonymous_user_id = UserId::new();
                    self.store_anon_call_history_failed(
                        &channel_id,
                        &pending.room_name,
                        &anonymous_user_id,
                        &pending.display_name,
                        pending.is_outgoing,
                    );
                }
                CallSignalingError::AnonCall(format!("Failed to join room: {}", e))
            })?;

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
            )
            .await
            .map_err(|e| {
                // Store failed call history
                let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                if let Ok(ch_id) = channel_id_str.parse::<ChannelId>() {
                    let anonymous_user_id = UserId::new();
                    self.store_anon_call_history_failed(
                        &ch_id,
                        &pending.room_name,
                        &anonymous_user_id,
                        &pending.display_name,
                        pending.is_outgoing,
                    );
                }
                CallSignalingError::CallWorker(e.to_string())
            })?;

        tracing::info!(room_name = %room_name, "Connected to LiveKit for anonymous call (caller side)");

        let verification_emojis = Self::anon_verification_emojis(&pending.encryption_key, &pending.room_name);

        let result = AnonCallSetupResult {
            room_name: pending.room_name.clone(),
            channel_id,
            encryption_key: pending.encryption_key.clone(),
            verification_emojis,
            is_video: pending.is_video_call,
            peer_display_name: callee_display_name.to_string(),
        };

        // Re-store pending call so end_anon_call can find it for hangup/history
        let mut active_pending = pending;
        active_pending.started_at = Some(chrono::Utc::now());
        self.anon_dm_service.store_pending_anon_call(room_name.clone(), active_pending);

        Ok(Some(result))
    }

    /// Decline an incoming anonymous call: send decline answer, store missed call history.
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

        // Store missed call history
        let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
        if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
            let anonymous_user_id = UserId::new();
            self.store_anon_call_history_failed(
                &channel_id,
                &pending.room_name,
                &anonymous_user_id,
                &pending.display_name,
                pending.is_outgoing,
            );
        }

        Ok(())
    }

    /// Store a failed/missed/declined anonymous call history entry.
    fn store_anon_call_history_failed(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        initiator_id: &UserId,
        display_name: &str,
        _is_outgoing: bool,
    ) {
        use crate::domain::message::*;

        let call_msg = CallMessage {
            call_id: call_id.to_string(),
            call_type: CallMessageType::Missed,
            initiator_id: *initiator_id,
            initiator_name: display_name.to_string(),
            started_at: None,
            ended_at: Some(chrono::Utc::now()),
            duration: None,
            participant_count: 2,
        };

        let message = crate::domain::message::Message {
            id: crate::domain::ids::MessageId::new(),
            conversation_id: *channel_id,
            conversation_type: ConversationType::AnonGroup,
            sender_id: Some(*initiator_id),
            device_id: None,
            content: MessageContent::Call(call_msg),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
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
            tracing::error!(error = %e, "Failed to save anon call history");
        }
    }

    // ================================================================
    // End-call orchestration (hangup + history + leave)
    // ================================================================

    /// End an active DM/channel call: send hangup, store history, leave.
    ///
    /// `call_worker` must be provided since it's behind a OnceLock in the client.
    /// `duration_secs` comes from the UI timer (core doesn't track call duration).
    #[cfg(feature = "desktop-media")]
    pub async fn end_dm_call(
        &self,
        call_worker: &CallWorkerHandle,
        duration_secs: i64,
    ) -> Result<(), CallSignalingError> {
        let call = call_worker.active_call().await
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Send hangup notification to peer
        if let Err(e) = self.send_call_hangup(
            &call.channel_id, &call.id.to_string(), CallHangupReason::Normal,
        ).await {
            tracing::error!(error = %e, "Failed to send hangup notification");
        }

        // Resolve initiator name and store call history
        let current_user = self.channel_service.current_user_id()
            .unwrap_or_else(|_| UserId::from_uuid(uuid::Uuid::nil()));
        let is_outgoing = call.initiator_id == current_user;

        // Initiator name resolved by caller (ChatterClient facade) since we
        // don't have UserService. Pass empty string — store_dm_call_history
        // uses the initiator_id as fallback display.
        self.store_dm_call_history(
            &call.channel_id, &call.id.to_string(),
            &call.initiator_id, &call.initiator_id.to_string(),
            is_outgoing, duration_secs, false,
        );

        // Leave the call
        if let Err(e) = call_worker.leave_call().await {
            tracing::error!(error = %e, "Failed to leave call");
        }

        Ok(())
    }

    /// End an active anonymous call: send hangup, store history, leave.
    ///
    /// Handles both anonymous DM and anonymous group calls.
    /// Always disconnects from the LiveKit room, even if hangup signaling fails.
    #[cfg(feature = "desktop-media")]
    pub async fn end_anon_call(
        &self,
        call_worker: &CallWorkerHandle,
        duration_secs: i64,
    ) -> Result<Option<crate::domain::anon_dm::AnonDmId>, CallSignalingError> {
        // Get room name from active call state (not from Call.id which is a random UUID)
        let room_name = match call_worker.get_room_name().await {
            Some(name) => name,
            None => {
                // No active call — just ensure we're disconnected
                tracing::warn!("end_anon_call: no active call room name, forcing disconnect");
                call_worker.disconnect_local().await;
                return Ok(None);
            }
        };

        // Take the pending call info (re-stored by accept/answer flow)
        let pending = self.anon_dm_service.take_pending_anon_call(&room_name);

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
                        &group_id, &pending.call_room_id,
                        crate::proto::content_v1::anon_call_hangup::Reason::Normal,
                    ).await {
                        tracing::error!(error = %e, "Failed to send group call hangup");
                    }
                }
            } else {
                if let Err(e) = self.anon_dm_service.send_call_hangup(
                    &conversation_id, &pending.call_room_id,
                    crate::proto::content_v1::anon_call_hangup::Reason::Normal,
                ).await {
                    tracing::error!(error = %e, "Failed to send anonymous call hangup");
                }
            }

            // Store call history
            let duration_str = if duration_secs > 0 {
                format!("{}:{:02}", duration_secs / 60, duration_secs % 60)
            } else {
                "0:00".to_string()
            };
            let label = if pending.is_outgoing { "Outgoing call" } else { "Incoming call" };
            let text = format!("\u{1F4DE} {} \u{00B7} {}", label, duration_str);
            let content_json = format!(r#"{{"text":"{}"}}"#, text);
            let sent_at = crate::util::time::TimestampNanos::now().as_u64();
            let user_id = self.channel_service.current_user_id()
                .unwrap_or_else(|_| UserId::from_uuid(uuid::Uuid::nil()));

            if let Err(e) = self.anon_dm_service.store_message(&conversation_id, &user_id, 1, &content_json, sent_at) {
                tracing::error!(error = %e, "Failed to store anon call history on hangup");
            }

            Some(conversation_id)
        } else {
            tracing::warn!(room_name = %room_name, "No pending call context for end — hangup signal skipped");
            None
        };

        // Always leave/disconnect from the LiveKit room
        if let Err(e) = call_worker.leave_call().await {
            tracing::debug!(error = %e, "leave_call after anon hangup");
        }

        Ok(conversation_id)
    }

    /// Handle remote hangup: store history, disconnect locally.
    #[cfg(feature = "desktop-media")]
    pub async fn handle_remote_hangup(
        &self,
        call_worker: &CallWorkerHandle,
        duration_secs: i64,
    ) -> Result<(), CallSignalingError> {
        if let Some(call) = call_worker.active_call().await {
            let current_user = self.channel_service.current_user_id()
                .unwrap_or_else(|_| UserId::from_uuid(uuid::Uuid::nil()));
            let is_outgoing = call.initiator_id == current_user;

            self.store_dm_call_history(
                &call.channel_id, &call.id.to_string(),
                &call.initiator_id, &call.initiator_id.to_string(),
                is_outgoing, duration_secs, false,
            );
        }

        call_worker.disconnect_local().await;
        Ok(())
    }

    /// Store a DM/channel call history message.
    #[cfg(feature = "desktop-media")]
    fn store_dm_call_history(
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
            conversation_type: ConversationType::AnonGroup,
            sender_id: Some(*initiator_id),
            device_id: None,
            content: MessageContent::Call(call_msg),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
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
