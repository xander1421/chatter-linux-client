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

use crate::crypto::call_key_provider::EphemeralCallKey;
use crate::domain::call::{CallSetupResult, CallType, PendingDmCall};
use crate::domain::ids::{CallId, ChannelId, UserId};
use crate::services::call_verification::CallVerification;
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::gateway::{
    build_call_key_offer, build_call_key_answer,
    build_call_hangup_content, build_group_call_key_content,
    CallHangupReason,
};
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
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
}

/// Handles call signaling (key exchange, hangup) over the Signal Protocol.
#[derive(Clone)]
pub struct CallSignalingService {
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
    channel_service: Arc<ChannelService>,
    /// Pending outgoing DM call state (waiting for answer with peer's ephemeral key).
    pending_dm_call: Arc<Mutex<Option<PendingDmCall>>>,
}

impl CallSignalingService {
    pub fn new(
        message_service: Arc<MessageService>,
        mailbox_manager: Arc<MailboxManager>,
        channel_service: Arc<ChannelService>,
    ) -> Self {
        Self {
            message_service,
            mailbox_manager,
            channel_service,
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
}
