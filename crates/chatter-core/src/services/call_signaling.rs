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

use crate::domain::ids::{ChannelId, UserId};
use crate::services::channel::ChannelService;
use crate::services::gateway::{
    build_call_key_offer, build_call_key_answer,
    build_call_hangup_content, build_group_call_key_content,
    CallHangupReason,
};
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;

/// Errors from call signaling operations.
#[derive(Debug, thiserror::Error)]
pub enum CallSignalingError {
    #[error("message send failed: {0}")]
    Send(#[from] crate::services::message::MessageError),
    #[error("channel error: {0}")]
    Channel(#[from] crate::services::channel::ChannelError),
    #[error("no DM peer found for channel {0}")]
    NoPeer(ChannelId),
}

/// Handles call signaling (key exchange, hangup) over the Signal Protocol.
#[derive(Clone)]
pub struct CallSignalingService {
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
    channel_service: Arc<ChannelService>,
}

impl CallSignalingService {
    pub fn new(
        message_service: Arc<MessageService>,
        mailbox_manager: Arc<MailboxManager>,
        channel_service: Arc<ChannelService>,
    ) -> Self {
        Self { message_service, mailbox_manager, channel_service }
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
}
