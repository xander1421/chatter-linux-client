//! Anonymous call signaling within private groups.
//!
//! Enables anonymous voice/video calls using blind signature tokens.
//! Calls are signaled through the existing anon group encrypted channel,
//! fanning out to all group members.

use uuid::Uuid;

use crate::domain::anon_group::AnonGroupId;
use crate::mailbox::CallRoomToken;
use crate::proto::content_v1::{
    self, AnonCallAnswer, AnonCallHangup, AnonCallOffer, AnonCallToken, Content,
    anon_call_offer::CallType,
};
use crate::services::anon_dm::{AnonCallOfferResult, AnonCallRoomCredentials};
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Initiate an anonymous call in a private group.
    ///
    /// Creates a call room and sends the offer to all group members.
    /// Each member receives the room name and encryption key.
    pub async fn send_call_offer(
        &self,
        group_id: &AnonGroupId,
        token: &CallRoomToken,
        video: bool,
        display_name: Option<&str>,
    ) -> Result<AnonCallOfferResult, AnonGroupError> {
        // Check if group is blocked
        self.check_group_not_blocked(group_id)?;

        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

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
            padding: Self::generate_padding(512),
        };

        // Send to all group members via group messaging
        self.send_message(group_id, content).await?;

        tracing::info!(
            group_id = %group_id,
            room_name = %room_name,
            call_type = ?if video { "video" } else { "voice" },
            "Sent anonymous group call offer"
        );

        Ok(AnonCallOfferResult {
            call_room_id: call_room_bytes,
            room_name,
            call_encryption_key,
            timestamp,
        })
    }

    /// Send a call answer in a private group (for notification purposes).
    pub async fn send_call_answer(
        &self,
        group_id: &AnonGroupId,
        call_room_id: &[u8; 16],
        accept: bool,
        token: Option<&CallRoomToken>,
        display_name: Option<&str>,
    ) -> Result<u64, AnonGroupError> {
        self.check_group_not_blocked(group_id)?;

        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        let timestamp = TimestampNanos::now().as_u64();

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
            decline_reason: 0,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonCallAnswer(answer)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(512),
        };

        self.send_message(group_id, content).await?;

        tracing::info!(
            group_id = %group_id,
            accepted = accept,
            "Sent anonymous group call answer"
        );

        Ok(timestamp)
    }

    /// Send a call hangup notification in a private group.
    pub async fn send_call_hangup(
        &self,
        group_id: &AnonGroupId,
        call_room_id: &[u8; 16],
        reason: content_v1::anon_call_hangup::Reason,
    ) -> Result<(), AnonGroupError> {
        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

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

        self.send_message(group_id, content).await?;

        tracing::info!(
            group_id = %group_id,
            reason = ?reason,
            "Sent anonymous group call hangup"
        );

        Ok(())
    }

    /// Join an anonymous call room using a blind signature token.
    ///
    /// Same mechanism as anon DM calls - the server verifies the token
    /// and returns LiveKit credentials.
    pub async fn join_anon_call_room(
        &self,
        room_name: &str,
        token: &CallRoomToken,
        display_name: &str,
        video: bool,
    ) -> Result<AnonCallRoomCredentials, AnonGroupError> {
        // Use anonymous mailbox client from pool (no auth headers, creates lazy channel if needed)
        let mut client = self.grpc_pool.anonymous_mailbox_client();

        let call_type = if video { 2 } else { 1 };

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
            .map_err(|e| AnonGroupError::CryptoError(format!("Failed to join call room: {}", e)))?;

        if !response.success {
            return Err(AnonGroupError::CryptoError(format!(
                "Server rejected call room join: {}",
                response.error
            )));
        }

        tracing::info!(
            room_name = %room_name,
            "Successfully joined anonymous group call room"
        );

        Ok(AnonCallRoomCredentials {
            livekit_token: response.livekit_token,
            livekit_url: response.livekit_url,
            expires_at: response.expires_at,
        })
    }
}
