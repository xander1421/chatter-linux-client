use chrono::Utc;
use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmStatus, AnonDmMessage as DomainAnonDmMessage};
use crate::domain::UserId;
use crate::proto::content_v1::{self, AnonDmAccept, Content};
use crate::util::time::TimestampNanos;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Accept an incoming message request.
    ///
    /// Creates our mailbox and sends acceptance to initiator's anonymous mailbox.
    pub async fn accept_request(
        &self,
        conversation_id: AnonDmId,
    ) -> Result<AnonDm, AnonDmError> {
        let _current_user = self.current_user_id()?;

        // Get the conversation
        let mut dm = self
            .anon_dm_repo
            .get_conversation(&conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(conversation_id))?;

        // Must be an incoming request
        if dm.initiated_by_us || dm.status != AnonDmStatus::Pending {
            return Err(AnonDmError::InvalidData(
                "Not a pending incoming request".to_string(),
            ));
        }

        // Create our mailbox for this conversation
        let mailbox = self.create_conversation_mailbox(&conversation_id).await?;
        self.anon_dm_repo.insert_mailbox(&mailbox)?;

        // Get peer's mailbox to send acceptance
        let peer_mailbox_pk = dm
            .peer_mailbox_pk
            .ok_or_else(|| AnonDmError::InvalidData("No peer mailbox".to_string()))?;

        // Get profile key if available
        let profile_key = self.get_my_profile_key();

        // Get our display name
        let my_display_name = self.get_my_display_name().unwrap_or_default();

        // Send acceptance to initiator's anonymous mailbox
        self.send_acceptance(
            &dm.peer_id,
            &conversation_id,
            &peer_mailbox_pk,
            &mailbox.public_key,
            profile_key.as_ref(),
            &my_display_name,
        )
        .await?;

        // Update conversation status
        dm.status = AnonDmStatus::Active;
        dm.updated_at = Utc::now();
        self.anon_dm_repo.update_conversation(&dm)?;

        tracing::info!(
            "Accepted anonymous DM conversation {} from {}",
            conversation_id,
            dm.peer_id
        );

        Ok(dm)
    }

    /// Send acceptance to initiator's anonymous mailbox.
    pub(crate) async fn send_acceptance(
        &self,
        peer_id: &UserId,
        conversation_id: &AnonDmId,
        peer_mailbox_pk: &[u8; 32],
        our_mailbox_pk: &[u8; 32],
        profile_key: Option<&[u8; 32]>,
        display_name: &str,
    ) -> Result<(), AnonDmError> {
        // Encrypt display name and avatar with profile key if available
        // (matches AnonDmRequest encryption for consistency)
        let (encrypted_display_name, encrypted_avatar) = if let Some(pk) = profile_key {
            // Encrypt display name
            let enc_name = if !display_name.is_empty() {
                crate::crypto::profile_crypto::encrypt_display_name(pk, display_name).ok()
            } else {
                None
            };
            // Encrypt avatar (stored locally, shared encrypted during bootstrap)
            let enc_avatar = self.get_encrypted_avatar(pk);
            (enc_name, enc_avatar)
        } else {
            (None, None)
        };

        let accept = AnonDmAccept {
            conversation_id: conversation_id.to_vec(),
            responder_mailbox_pk: our_mailbox_pk.to_vec(),
            profile_key: profile_key.map(|k| k.to_vec()),
            timestamp: TimestampNanos::now().as_u64(),
            responder_display_name: display_name.to_string(),
            encrypted_display_name,
            encrypted_avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmAccept(accept)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to initiator's anonymous mailbox
        self.send_to_anon_mailbox(peer_id, peer_mailbox_pk, content)
            .await?;

        tracing::debug!(
            "Sent acceptance for conversation {} to {}",
            conversation_id,
            peer_id
        );
        Ok(())
    }

    /// Decline an incoming message request.
    pub fn decline_request(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        // Must be an incoming request
        if dm.initiated_by_us || dm.status != AnonDmStatus::Pending {
            return Err(AnonDmError::InvalidData(
                "Not a pending incoming request".to_string(),
            ));
        }

        // Update status to declined
        self.anon_dm_repo
            .update_status(conversation_id, AnonDmStatus::Declined)?;

        tracing::info!(
            "Declined anonymous DM conversation {} from {}",
            conversation_id,
            dm.peer_id
        );

        Ok(())
    }

    /// Handle an acceptance from the recipient.
    ///
    /// Updates the conversation with peer's mailbox, display name, and activates it.
    pub fn handle_acceptance(
        &self,
        conversation_id: AnonDmId,
        peer_id: UserId,
        responder_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        peer_display_name: Option<String>,
        peer_avatar: Option<Vec<u8>>,
    ) -> Result<(), AnonDmError> {
        let mut dm = self
            .anon_dm_repo
            .get_conversation(&conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(conversation_id))?;

        // Verify this is our outgoing request
        if !dm.initiated_by_us || dm.peer_id != peer_id {
            return Err(AnonDmError::InvalidData(
                "Acceptance from unexpected peer".to_string(),
            ));
        }

        // Update with peer's mailbox, display name, avatar, and activate
        dm.peer_mailbox_pk = Some(responder_mailbox_pk);
        dm.peer_profile_key = profile_key;
        dm.peer_display_name = peer_display_name;
        dm.peer_avatar = peer_avatar;
        dm.status = AnonDmStatus::Active;
        dm.updated_at = Utc::now();
        self.anon_dm_repo.update_conversation(&dm)?;

        tracing::info!(
            "Received acceptance for conversation {} from {}",
            conversation_id,
            peer_id
        );

        Ok(())
    }

    /// Handle an incoming message request.
    ///
    /// Creates a pending conversation that can be accepted or declined.
    pub fn handle_request(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_mailbox_pk: [u8; 32],
        intro_message: Option<&str>,
        profile_key: Option<[u8; 32]>,
        peer_display_name: Option<String>,
        peer_avatar: Option<Vec<u8>>,
    ) -> Result<AnonDm, AnonDmError> {
        let current_user = self.current_user_id()?;

        // Can't message yourself
        if sender_id == current_user {
            return Err(AnonDmError::CannotMessageSelf);
        }

        // Check if conversation already exists
        if let Some(existing) = self.anon_dm_repo.get_conversation(&conversation_id)? {
            // Already have this conversation
            return Ok(existing);
        }

        // Create conversation as incoming request
        let mut dm = AnonDm::from_request(conversation_id, sender_id, sender_mailbox_pk);
        dm.peer_profile_key = profile_key;
        dm.peer_display_name = peer_display_name;
        dm.peer_avatar = peer_avatar;
        self.anon_dm_repo.insert_conversation(&dm)?;

        // Store the intro message if present
        if let Some(text) = intro_message {
            if !text.is_empty() {
                // Use current time as sent_at for intro messages (they don't have a Content timestamp)
                let sent_at = TimestampNanos::now().as_u64();
                let msg = DomainAnonDmMessage::new_text(conversation_id, sender_id, text, sent_at);
                self.anon_dm_repo.insert_message(&msg)?;
            }
        }

        tracing::info!(
            "Received message request {} from {}",
            conversation_id,
            sender_id
        );

        Ok(dm)
    }

    /// Create an incoming conversation from ECIES init message (NDSS'21 private contact).
    ///
    /// This is called when we receive a Phase 1 HPKE-encrypted initial contact message.
    /// Stores the conversation and peer's keys for Phase 2 Double Ratchet setup.
    pub fn create_incoming_conversation(
        &self,
        dm: &AnonDm,
        _peer_identity_key: &[u8; 33],
        _peer_delivery_token: &[u8; 12],
    ) -> Result<(), AnonDmError> {
        // Check if conversation already exists
        if let Some(_existing) = self.anon_dm_repo.get_conversation(&dm.id)? {
            tracing::warn!(
                conversation_id = %dm.id,
                "Conversation already exists from ECIES init"
            );
            return Ok(());
        }

        // Store the conversation
        self.anon_dm_repo.insert_conversation(dm)?;

        tracing::info!(
            conversation_id = %dm.id,
            peer_id = %dm.peer_id,
            "Created incoming conversation from ECIES init message"
        );

        Ok(())
    }
}
