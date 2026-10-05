//! Anonymous call signaling within anon DM conversations.
//!
//! Enables anonymous voice/video calls using blind signature tokens.
//! Calls are signaled through the existing anon DM encrypted channel.

use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::domain::anon_dm::AnonDmId;
use crate::mailbox::CallRoomToken;
use crate::proto::content_v1::{
    self, AnonCallAnswer, AnonCallHangup, AnonCallOffer, AnonCallToken, Content,
    anon_call_answer::DeclineReason, anon_call_offer::CallType,
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

/// The room of our dialed, unanswered call among `calls` (only if it is
/// `room_name`'s, when given). An answer marks a call started; an incoming
/// call is never ours to cancel.
fn ringing_room(
    calls: &HashMap<String, PendingAnonCall>,
    room_name: Option<&str>,
) -> Option<String> {
    calls
        .iter()
        .find(|(room, call)| {
            call.is_outgoing
                && call.started_at.is_none()
                && room_name.is_none_or(|r| r == room.as_str())
        })
        .map(|(room, _)| room.clone())
}

/// Whether `call` is `sender` ringing us: incoming, unanswered, theirs.
fn rings_us(call: &PendingAnonCall, sender: crate::domain::UserId) -> bool {
    !call.is_outgoing && call.started_at.is_none() && call.sender_id == Some(sender)
}

#[cfg(test)]
mod ringing_room_tests {
    use super::*;

    fn call(room: &str, is_outgoing: bool, answered: bool) -> (String, PendingAnonCall) {
        let token = CallRoomToken {
            id: String::new(),
            public_key: vec![],
            private_key: vec![],
            signature: vec![],
            randomizer: None,
        };
        (
            room.to_string(),
            PendingAnonCall {
                call_room_id: [0; 16],
                room_name: room.to_string(),
                conversation_id: AnonDmId::new(),
                token,
                is_video_call: false,
                is_outgoing,
                display_name: String::new(),
                encryption_key: vec![],
                sender_id: None,
                started_at: answered.then(chrono::Utc::now),
            },
        )
    }

    #[test]
    fn only_our_unanswered_call_is_ringing() {
        let calls: HashMap<_, _> = [call("incoming", false, false), call("answered", true, true)]
            .into_iter()
            .collect();
        assert_eq!(ringing_room(&calls, None), None);

        let mut calls = calls;
        calls.extend([call("ringing", true, false)]);
        assert_eq!(ringing_room(&calls, None).as_deref(), Some("ringing"));
        assert_eq!(
            ringing_room(&calls, Some("ringing")).as_deref(),
            Some("ringing")
        );
        // The ring timeout of another call must not end this one.
        assert_eq!(ringing_room(&calls, Some("answered")), None);
    }

    #[test]
    fn only_the_ringer_calls_off_our_ring() {
        let ringer = crate::domain::UserId::new();
        let (_, mut ring) = call("room", false, false);
        ring.sender_id = Some(ringer);
        assert!(rings_us(&ring, ringer));
        // Another group member's hangup leaves the ring alone.
        assert!(!rings_us(&ring, crate::domain::UserId::new()));
        let (_, mut accepted) = call("room", false, true);
        accepted.sender_id = Some(ringer);
        assert!(!rings_us(&accepted, ringer));
        let (_, mut ours) = call("room", true, false);
        ours.sender_id = Some(ringer);
        assert!(!rings_us(&ours, ringer));
    }
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

    /// Take our outgoing call that is still ringing — dialed, not yet
    /// answered — optionally only if it is `room_name`'s. Checked and removed
    /// under one lock, so an answer being processed and a cancel or ring
    /// timeout cannot both claim the call.
    pub fn take_ringing_anon_call(&self, room_name: Option<&str>) -> Option<PendingAnonCall> {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        let room = ringing_room(&calls, room_name)?;
        calls.remove(&room)
    }

    /// Take the incoming call `sender` is still ringing us with in
    /// `room_name` — offered, not yet accepted — so a ring called off leaves
    /// nothing behind. `None` for any other call in that room: ours, one we
    /// accepted, or (in a group) one another member rang.
    pub fn take_incoming_anon_call(
        &self,
        room_name: &str,
        sender: crate::domain::UserId,
    ) -> Option<PendingAnonCall> {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        calls.get(room_name).filter(|c| rings_us(c, sender))?;
        calls.remove(room_name)
    }

    /// Clear a pending anonymous call by room name.
    pub fn clear_pending_anon_call(&self, room_name: &str) {
        let mut calls = PENDING_ANON_CALLS.lock().unwrap();
        calls.remove(room_name);
    }

    /// Find the room name of an active anonymous call by its conversation id.
    ///
    /// `AnonCallHangupReceived` carries only the conversation id, but the
    /// orchestrator keys active calls by `room_name`. This lookup bridges
    /// the two so the orchestrator can mark the right call ended.
    pub fn find_anon_room_for_conversation(&self, conversation_id: &AnonDmId) -> Option<String> {
        let calls = PENDING_ANON_CALLS.lock().unwrap();
        calls
            .iter()
            .find(|(_, call)| call.conversation_id == *conversation_id)
            .map(|(room_name, _)| room_name.clone())
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
        // SECURITY: call E2EE key must draw from the OS entropy pool, not a
        // once-seeded userspace PRG (ThreadRng). Matches call_key_provider.
        {
            use rand::TryRngCore;
            rand::RngCore::fill_bytes(
                &mut rand::rngs::OsRng.unwrap_err(),
                &mut call_encryption_key,
            );
        }

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
            // Our real name only when visibility lets anon peers hold our
            // profile key; otherwise they see "Anonymous", as for any stranger.
            caller_display_name: display_name
                .filter(|_| self.get_my_profile_key().is_some())
                .unwrap_or_default()
                .to_string(),
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
    ) -> Result<u64, AnonDmError> {
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
            // Our real name only when visibility lets anon peers hold our
            // profile key; otherwise they see "Anonymous", as for any stranger.
            callee_display_name: display_name
                .filter(|_| self.get_my_profile_key().is_some())
                .unwrap_or_default()
                .to_string(),
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

        Ok(timestamp)
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
        // Use anonymous mailbox client from pool (no auth headers, creates lazy channel if needed)
        let mut client = self.grpc_pool.anonymous_mailbox_client();

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
