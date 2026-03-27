//! Anonymous call signaling within anon DM conversations.
//!
//! Enables anonymous voice/video calls using blind signature tokens.
//! Calls are signaled through the existing anon DM encrypted channel.

use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::domain::anon_dm::AnonDmId;
use crate::mailbox::CallRoomToken;
use crate::network::grpc::AnonymousMailboxClient;
use crate::proto::content_v1::{
    self, anon_call_answer::DeclineReason, anon_call_offer::CallType, AnonCallAnswer,
    AnonCallHangup, AnonCallOffer, AnonCallToken, Content,
};
use crate::util::time::TimestampNanos;

use super::{AnonDmError, AnonDmService};

/// State for a pending anonymous call.
#[derive(Clone)]
pub struct PendingAnonCall {
    /// The call room ID.
    pub call_room_id: [u8; 16],
    /// The room name for LiveKit.
    pub room_name: String,
    /// The anon DM conversation ID.
    pub conversation_id: AnonDmId,
    /// The blind signature token used.
    pub token: CallRoomToken,
    /// Whether this is a video call.
    pub is_video_call: bool,
    /// Whether we initiated the call.
    pub is_outgoing: bool,
    /// Display name for the call.
    pub display_name: String,
    /// E2EE encryption key for LiveKit (32 bytes).
    pub encryption_key: Vec<u8>,
    /// Sender ID (for incoming calls).
    pub sender_id: Option<crate::domain::UserId>,
    /// When the call started (for duration tracking).
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// LiveKit credentials for joining an anonymous call room.
#[derive(Debug, Clone)]
pub struct AnonCallRoomCredentials {
    /// LiveKit access token (JWT).
    pub livekit_token: String,
    /// LiveKit server URL.
    pub livekit_url: String,
    /// When the token expires.
    pub expires_at: u64,
}

/// Result of initiating an anonymous call.
#[derive(Debug, Clone)]
pub struct AnonCallOfferResult {
    /// The unique call room ID.
    pub call_room_id: [u8; 16],
    /// The room name (for LiveKit).
    pub room_name: String,
    /// E2EE encryption key (32 bytes).
    pub call_encryption_key: Vec<u8>,
    /// When the offer was sent.
    pub timestamp: u64,
}

/// Received call offer from peer.
#[derive(Debug, Clone)]
pub struct ReceivedCallOffer {
    /// The unique call room ID.
    pub call_room_id: [u8; 16],
    /// The room name (for LiveKit).
    pub room_name: String,
    /// Caller's blind signature token.
    pub caller_token: AnonCallToken,
    /// Call type (voice/video).
    pub call_type: CallType,
    /// Caller's display name.
    pub caller_display_name: String,
    /// When the offer was received.
    pub timestamp: u64,
}

/// Received call answer from peer.
#[derive(Debug, Clone)]
pub struct ReceivedCallAnswer {
    /// The call room ID being answered.
    pub call_room_id: [u8; 16],
    /// Whether the call was accepted.
    pub accepted: bool,
    /// Callee's blind signature token (if accepted).
    pub callee_token: Option<AnonCallToken>,
    /// Decline reason (if not accepted).
    pub decline_reason: Option<DeclineReason>,
    /// Callee's display name.
    pub callee_display_name: String,
    /// When the answer was received.
    pub timestamp: u64,
}

// Thread-safe storage for pending anonymous calls.
// Key is the room name.
static PENDING_ANON_CALLS: std::sync::LazyLock<Mutex<HashMap<String, PendingAnonCall>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

impl AnonDmService {
    /// Store a pending anonymous call.
    pub fn store_pending_anon_call(&self, room_name: String, call: PendingAnonCall) {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        calls.insert(room_name, call);
    }
    
    /// Get a pending anonymous call by room name (does not consume).
    pub fn get_pending_anon_call(&self, room_name: &str) -> Option<PendingAnonCall> {
        let calls = PENDING_ANON_CALLS.lock().unwrap();
        calls.get(room_name).cloned()
    }
    
    /// Take a pending anonymous call by room name (consumes and removes).
    pub fn take_pending_anon_call(&self, room_name: &str) -> Option<PendingAnonCall> {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        calls.remove(room_name)
    }
    
    /// Clear a pending anonymous call by room name.
    pub fn clear_pending_anon_call(&self, room_name: &str) {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        calls.remove(room_name);
    }
    
    /// Mark an anonymous call as started (sets the start time).
    pub fn mark_anon_call_started(&self, room_name: &str) {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        if let Some(call) = calls.get_mut(room_name) {
            call.started_at = Some(chrono::Utc::now());
        }
    }
    /// Initiate an anonymous call to the peer in this conversation.
    ///
    /// Uses a blind signature token to prove authorization without revealing identity.
    /// The caller generates a unique room ID and sends it along with their token.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The anon DM conversation
    /// * `token` - The caller's blind signature token
    /// * `video` - True for video call, false for voice-only
    /// * `display_name` - Optional display name for the call
    ///
    /// # Returns
    ///
    /// The call room ID and room name to use with JoinCallRoom RPC.
    pub async fn send_call_offer(
        &self,
        conversation_id: &AnonDmId,
        token: &CallRoomToken,
        video: bool,
        display_name: Option<&str>,
    ) -> Result<AnonCallOfferResult, AnonDmError> {
        // Check if conversation is blocked
        self.check_conversation_not_blocked(conversation_id)?;

        // Get conversation and verify it's active
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if !dm.is_ready() {
            return Err(AnonDmError::ConversationNotReady(*conversation_id));
        }

        // Generate unique call room ID
        let call_room_id = Uuid::new_v4();
        let call_room_bytes: [u8; 16] = *call_room_id.as_bytes();
        let room_name = format!("anon-{}", call_room_id);

        let timestamp = TimestampNanos::now().as_u64();

        // Generate random 32-byte E2EE key for this call
        let mut call_encryption_key = vec![0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut call_encryption_key);

        // Create the call offer
        let offer = AnonCallOffer {
            call_room_id: call_room_bytes.to_vec(),
            room_name: room_name.clone(),
            caller_token: Some(AnonCallToken {
                public_key: token.public_key.clone(),
                signature: token.signature.clone(),
                message_randomizer: token.randomizer.clone().unwrap_or_default(),
            }),
            call_type: if video {
                CallType::Video.into()
            } else {
                CallType::Voice.into()
            },
            timestamp,
            caller_display_name: display_name.unwrap_or_default().to_string(),
            call_encryption_key: call_encryption_key.clone(),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonCallOffer(offer)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(512), // Larger padding for call signaling
        };

        // Send via anon DM channel
        self.send_message(conversation_id, content).await?;

        tracing::info!(
            conversation_id = %conversation_id,
            room_name = %room_name,
            call_type = ?if video { "video" } else { "voice" },
            "Sent anonymous call offer"
        );

        Ok(AnonCallOfferResult {
            call_room_id: call_room_bytes,
            room_name,
            call_encryption_key,
            timestamp,
        })
    }

    /// Accept or decline an anonymous call offer.
    ///
    /// If accepting, the callee provides their own blind signature token
    /// to use for joining the call room.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The anon DM conversation
    /// * `call_room_id` - The call room ID from the offer
    /// * `accept` - True to accept, false to decline
    /// * `token` - The callee's blind signature token (required if accepting)
    /// * `decline_reason` - Reason for declining (if not accepting)
    /// * `display_name` - Optional display name for the call
    pub async fn send_call_answer(
        &self,
        conversation_id: &AnonDmId,
        call_room_id: &[u8; 16],
        accept: bool,
        token: Option<&CallRoomToken>,
        decline_reason: Option<DeclineReason>,
        display_name: Option<&str>,
    ) -> Result<(), AnonDmError> {
        // Check if conversation is blocked
        self.check_conversation_not_blocked(conversation_id)?;

        // Get conversation and verify it's active
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if !dm.is_ready() {
            return Err(AnonDmError::ConversationNotReady(*conversation_id));
        }

        let timestamp = TimestampNanos::now().as_u64();

        // Create the call answer
        let answer = AnonCallAnswer {
            call_room_id: call_room_id.to_vec(),
            accepted: accept,
            callee_token: if accept {
                token.map(|t| AnonCallToken {
                    public_key: t.public_key.clone(),
                    signature: t.signature.clone(),
                    message_randomizer: t.randomizer.clone().unwrap_or_default(),
                })
            } else {
                None
            },
            timestamp,
            callee_display_name: display_name.unwrap_or_default().to_string(),
            decline_reason: decline_reason.map(|r| r.into()).unwrap_or(0),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonCallAnswer(answer)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(512),
        };

        // Send via anon DM channel
        self.send_message(conversation_id, content).await?;

        tracing::info!(
            conversation_id = %conversation_id,
            accepted = accept,
            "Sent anonymous call answer"
        );

        Ok(())
    }

    /// End an anonymous call.
    ///
    /// Sends a hangup notification to the peer.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The anon DM conversation
    /// * `call_room_id` - The call room ID
    /// * `reason` - Reason for hanging up
    pub async fn send_call_hangup(
        &self,
        conversation_id: &AnonDmId,
        call_room_id: &[u8; 16],
        reason: content_v1::anon_call_hangup::Reason,
    ) -> Result<(), AnonDmError> {
        // Get conversation (don't check blocked - allow hangup even if blocked)
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if !dm.is_ready() {
            return Err(AnonDmError::ConversationNotReady(*conversation_id));
        }

        let timestamp = TimestampNanos::now().as_u64();

        let hangup = AnonCallHangup {
            call_room_id: call_room_id.to_vec(),
            reason: reason.into(),
            timestamp,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonCallHangup(hangup)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(256),
        };

        // Send via anon DM channel
        self.send_message(conversation_id, content).await?;

        tracing::info!(
            conversation_id = %conversation_id,
            reason = ?reason,
            "Sent anonymous call hangup"
        );

        Ok(())
    }

    /// Join an anonymous call room using a blind signature token.
    ///
    /// This contacts the server to verify the token and get LiveKit credentials.
    /// The server cannot link this to your identity.
    ///
    /// # Arguments
    ///
    /// * `room_name` - The room name (must start with "anon-")
    /// * `token` - The blind signature token
    /// * `display_name` - Display name for this participant
    /// * `video` - True for video call, false for voice-only
    ///
    /// # Returns
    ///
    /// LiveKit credentials for joining the call.
    pub async fn join_anon_call_room(
        &self,
        room_name: &str,
        token: &CallRoomToken,
        display_name: &str,
        video: bool,
    ) -> Result<AnonCallRoomCredentials, AnonDmError> {
        // Get channel (no auth interceptor used by AnonymousMailboxClient)
        let channel = self.grpc_pool.try_channel().ok_or_else(|| {
            AnonDmError::MessageError("Not connected to server".to_string())
        })?;

        let mut client = AnonymousMailboxClient::new(channel);

        let call_type = if video { 2 } else { 1 }; // 1 = voice, 2 = video

        let response = client
            .join_anon_call_room(
                room_name.to_string(),
                token.public_key.clone(),
                token.signature.clone(),
                token.randomizer.clone().unwrap_or_default(),
                display_name.to_string(),
                call_type,
            )
            .await
            .map_err(|e| AnonDmError::MessageError(format!("Failed to join call room: {}", e)))?;

        if !response.success {
            let error = response.error;
            return Err(AnonDmError::MessageError(format!(
                "Server rejected call room join: {}",
                error
            )));
        }

        tracing::info!(
            room_name = %room_name,
            "Successfully joined anonymous call room"
        );

        Ok(AnonCallRoomCredentials {
            livekit_token: response.livekit_token,
            livekit_url: response.livekit_url,
            expires_at: response.expires_at,
        })
    }
}
