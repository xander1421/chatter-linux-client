use crate::domain::UserId;
use crate::domain::anon_dm::{AnonDm, AnonDmId, AnonDmStatus};
use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::proto::content_v1::{self, AnonDmAccept, Content};
use crate::util::time::TimestampNanos;
use chrono::Utc;

use super::{AnonDmError, AnonDmService, DecryptedPeerProfile};

impl AnonDmService {
    /// Accept an incoming message request.
    ///
    /// Creates our mailbox and sends acceptance to initiator's anonymous mailbox.
    /// The acceptance never waits on a first avatar upload over Tor (RC108
    /// O3, as the request): an avatar not uploaded yet follows in a profile
    /// update sent in the background once the chat is active.
    pub async fn accept_request(
        self: &std::sync::Arc<Self>,
        conversation_id: AnonDmId,
    ) -> Result<AnonDm, AnonDmError> {
        let _current_user = self.current_user_id()?;

        // Get the conversation
        let dm = self
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
        let avatar_owed = self
            .send_acceptance(
                &dm,
                &peer_mailbox_pk,
                &mailbox.public_key,
                profile_key.as_ref(),
                &my_display_name,
            )
            .await?;

        // Status alone: `dm` is a snapshot from before the sends above, and a
        // peer avatar fetched meanwhile must not be overwritten with it.
        self.anon_dm_repo
            .update_status(&conversation_id, AnonDmStatus::Active)?;
        let dm = self
            .anon_dm_repo
            .get_conversation(&conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(conversation_id))?;

        // Secret chat: no peer identity in the log.
        tracing::info!(%conversation_id, "Accepted secret chat");

        if avatar_owed && tokio::runtime::Handle::try_current().is_ok() {
            let service = std::sync::Arc::clone(self);
            let name = Some(my_display_name).filter(|n| !n.is_empty());
            tokio::spawn(async move {
                // Ephemeral: logs and returns Ok on a failed send.
                let _ = service
                    .send_profile_update(&conversation_id, name, None, None)
                    .await;
            });
        }

        Ok(dm)
    }

    /// Send acceptance to initiator's anonymous mailbox. `Ok(true)` when our
    /// avatar is owed: it was never uploaded, so the acceptance went without
    /// it and a profile update must follow.
    pub(crate) async fn send_acceptance(
        &self,
        dm: &AnonDm,
        peer_mailbox_pk: &[u8; 32],
        our_mailbox_pk: &[u8; 32],
        profile_key: Option<&[u8; 32]>,
        display_name: &str,
    ) -> Result<bool, AnonDmError> {
        // Name under the profile key, avatar as a pointer (as in AnonDmRequest),
        // only an already-uploaded one: no upload over Tor here.
        let mut avatar_owed = false;
        let (encrypted_display_name, avatar) = if let Some(pk) = profile_key {
            // Encrypt display name
            let enc_name = if !display_name.is_empty() {
                crate::crypto::profile_crypto::encrypt_display_name(pk, display_name).ok()
            } else {
                None
            };
            let (cached, owed) = self.my_uploaded_avatar_pointer();
            avatar_owed = owed;
            (enc_name, cached)
        } else {
            (None, None)
        };

        let conversation_id = &dm.id;
        let accept = AnonDmAccept {
            conversation_id: conversation_id.to_vec(),
            responder_mailbox_pk: our_mailbox_pk.to_vec(),
            profile_key: profile_key.map(|k| k.to_vec()),
            timestamp: TimestampNanos::now().as_u64(),
            encrypted_display_name,
            avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonDmAccept(accept)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(256),
        };

        // Send to initiator's anonymous mailbox
        self.send_to_anon_mailbox(dm, peer_mailbox_pk, content)
            .await?;

        tracing::debug!(%conversation_id, "Sent secret chat acceptance");
        Ok(avatar_owed)
    }

    /// Decline an incoming message request: tell the initiator over Tor
    /// (an end signal into the mailbox their request announced, so their
    /// side shows it declined), then delete the request. Fails closed like
    /// "Delete chat": if that send fails nothing is deleted and the error is
    /// returned. A request whose mailbox is already gone has nobody left to
    /// tell and goes locally. We hold no mailbox of our own yet (one is
    /// created on accept), so the row is all there is.
    pub async fn decline_request(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
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

        if let Some(peer_mailbox_pk) = dm.peer_mailbox_pk {
            match self
                .send_to_anon_mailbox(
                    &dm,
                    &peer_mailbox_pk,
                    super::blocking::end_content(conversation_id),
                )
                .await
            {
                Ok(()) | Err(AnonDmError::PeerMailboxGone) => {}
                Err(e) => return Err(e),
            }
        }
        self.delete_conversation(conversation_id)?;

        tracing::info!(%conversation_id, "Declined secret chat");

        Ok(())
    }

    /// Handle an acceptance from the recipient.
    ///
    /// Updates the conversation with peer's mailbox, display name, and activates it.
    pub fn handle_acceptance(
        &self,
        conversation_id: AnonDmId,
        peer_id: UserId,
        // Sealed-sender-authenticated device the acceptance came from.
        peer_device_id: u32,
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

        // The chat is bound to the device our request was sealed to; an
        // acceptance from any other device is refused (a Shield ECIES request
        // went unbound, so the accepting device binds it here).
        self.anon_dm_repo
            .pin_peer_device(&conversation_id, peer_device_id)
            .map_err(|_| {
                AnonDmError::InvalidData("Acceptance from an unexpected device".to_string())
            })?;

        // Update with peer's mailbox, display name, avatar, and activate
        dm.peer_mailbox_pk = Some(responder_mailbox_pk);
        dm.peer_profile_key = profile_key;
        dm.peer_display_name = peer_display_name;
        dm.peer_avatar = peer_avatar;
        dm.status = AnonDmStatus::Active;
        dm.updated_at = Utc::now();
        self.anon_dm_repo.update_conversation(&dm)?;

        tracing::info!(%conversation_id, "Received secret chat acceptance");

        Ok(())
    }

    /// Handle an incoming message request.
    ///
    /// Creates a pending conversation that can be accepted or declined.
    pub fn handle_request(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        // Sealed-sender-authenticated device the request came from: the chat
        // is bound to it.
        sender_device_id: u32,
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

        // A request we already hold is a resend of it (the initiator resends
        // an uncertain request under the same id): nothing new to show.
        if self.anon_dm_repo.get_conversation(&conversation_id)?.is_some() {
            return Err(AnonDmError::ConversationExists(sender_id));
        }

        // Create conversation as incoming request
        let mut dm = AnonDm::from_request(conversation_id, sender_id, sender_mailbox_pk);
        dm.peer_device_id = Some(sender_device_id);
        dm.peer_profile_key = profile_key;
        dm.peer_display_name = peer_display_name;
        dm.peer_avatar = peer_avatar;
        self.anon_dm_repo.insert_conversation(&dm)?;

        // Store the intro message if present
        if let Some(text) = intro_message {
            if !text.is_empty() {
                // Use current time as sent_at for intro messages (they don't have a Content timestamp)
                let sent_at = TimestampNanos::now().as_u64();
                let channel_id: ChannelId = conversation_id.into();
                let created_at = TimestampNanos::from_nanos(sent_at)
                    .to_datetime()
                    .unwrap_or_else(Utc::now);
                let msg = Message {
                    id: MessageId::new(),
                    conversation_id: channel_id,
                    conversation_type: ConversationType::AnonDm,
                    sender_id: Some(sender_id),
                    device_id: None,
                    content: MessageContent::Text(text.to_string()),
                    encrypted_content: None,
                    thread_id: None,
                    reply_to: None,
                    reply_to_author_id: None,
                    reply_to_text: None,
                    reply_to_thumbnail: None,
                    reply_to_thumbnail_mime: None,
                    attachments: Vec::new(),
                    is_pinned: false,
                    is_edited: false,
                    is_deleted: false,
                    is_read: false,
                    sent_at,
                    created_at,
                    updated_at: created_at,
                    edited_at: None,
                    reaction_counts: Vec::new(),
                    current_user_reactions: Vec::new(),
                    mentioned_integrations: Vec::new(),
                    targets_ai_bot: false,
                    delivery_status: String::new(),
                    failure_reason: None,
                };
                self.message_repo.insert(&msg)?;
            }
        }

        tracing::info!(%conversation_id, "Received secret chat request");

        Ok(dm)
    }

    /// Create an incoming conversation from ECIES init message (NDSS'21 private contact).
    ///
    /// This is called when we receive a Phase 1 HPKE-encrypted initial contact message.
    /// Returns whether this request was newly stored; redelivery must not notify again.
    pub fn create_incoming_conversation(
        &self,
        dm: &AnonDm,
        _peer_identity_key: &[u8; 33],
        _peer_delivery_token: &[u8; 12],
    ) -> Result<bool, AnonDmError> {
        // Check if conversation already exists
        if let Some(_existing) = self.anon_dm_repo.get_conversation(&dm.id)? {
            tracing::warn!(
                conversation_id = %dm.id,
                "Conversation already exists from ECIES init"
            );
            return Ok(false);
        }

        // Store the conversation
        self.anon_dm_repo.insert_conversation(dm)?;

        tracing::info!(
            conversation_id = %dm.id,
            "Created incoming conversation from ECIES init message"
        );

        Ok(true)
    }

    /// Handle an incoming message request with encrypted profile data.
    ///
    /// This method accepts encrypted profile data and handles decryption in the
    /// service layer, keeping crypto operations out of UI code.
    ///
    /// # Returns
    ///
    /// On success, returns the created conversation and decrypted profile.
    /// On failure, returns an error (caller should log and ignore).
    #[allow(clippy::too_many_arguments)]
    pub fn handle_request_encrypted(
        &self,
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_device_id: u32,
        sender_mailbox_pk: [u8; 32],
        intro_message: Option<&str>,
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        // Already fetched from the sender's avatar pointer.
        avatar: Option<Vec<u8>>,
    ) -> Result<(crate::domain::anon_dm::AnonDm, DecryptedPeerProfile), AnonDmError> {
        use crate::crypto::profile_crypto;

        // Parse profile key
        let profile_key_arr = profile_key.and_then(|pk| {
            if pk.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&pk);
                Some(arr)
            } else {
                None
            }
        });

        // Decrypt display name - use "Unknown" as fallback
        let display_name = match (&profile_key_arr, &encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match profile_crypto::decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => name,
                    _ => "Unknown".to_string(),
                }
            }
            _ => "Unknown".to_string(),
        };

        tracing::debug!(
            has_avatar = avatar.is_some(),
            "Decrypted sender profile from AnonDmRequest"
        );

        let profile = DecryptedPeerProfile {
            display_name: display_name.clone(),
            avatar: avatar.clone(),
            profile_key: profile_key_arr,
        };

        // Store the request with decrypted data
        let dm = self.handle_request(
            conversation_id,
            sender_id,
            sender_device_id,
            sender_mailbox_pk,
            intro_message,
            profile_key_arr,
            Some(display_name),
            avatar,
        )?;

        Ok((dm, profile))
    }

    /// Handle an acceptance with encrypted profile data.
    ///
    /// This method accepts encrypted profile data and handles decryption in the
    /// service layer, keeping crypto operations out of UI code.
    ///
    /// # Returns
    ///
    /// On success, returns the decrypted profile.
    /// On failure, returns an error (caller should log and ignore).
    pub fn handle_acceptance_encrypted(
        &self,
        conversation_id: AnonDmId,
        peer_id: UserId,
        peer_device_id: u32,
        responder_mailbox_pk: [u8; 32],
        profile_key: Option<Vec<u8>>,
        encrypted_display_name: Option<Vec<u8>>,
        // Already fetched from the sender's avatar pointer.
        avatar: Option<Vec<u8>>,
    ) -> Result<DecryptedPeerProfile, AnonDmError> {
        use crate::crypto::profile_crypto;

        // Parse profile key
        let profile_key_arr = profile_key.and_then(|pk| {
            if pk.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&pk);
                Some(arr)
            } else {
                None
            }
        });

        // Decrypt display name - use "Unknown" as fallback
        let display_name = match (&profile_key_arr, &encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                match profile_crypto::decrypt_display_name(pk, encrypted) {
                    Ok(name) if !name.is_empty() => name,
                    _ => "Unknown".to_string(),
                }
            }
            _ => "Unknown".to_string(),
        };

        tracing::debug!(
            has_avatar = avatar.is_some(),
            "Decrypted responder profile from AnonDmAccept"
        );

        let profile = DecryptedPeerProfile {
            display_name: display_name.clone(),
            avatar: avatar.clone(),
            profile_key: profile_key_arr,
        };

        // Store the acceptance with decrypted data
        self.handle_acceptance(
            conversation_id,
            peer_id,
            peer_device_id,
            responder_mailbox_pk,
            profile_key_arr,
            Some(display_name),
            avatar,
        )?;

        Ok(profile)
    }
}
