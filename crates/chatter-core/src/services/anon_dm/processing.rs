use crate::domain::UserId;
use crate::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::ConversationType;
use crate::inbox::handlers::DomainEvent;
use crate::inbox::handlers::base_anon::{
    AnonInboxBackend, CommonDispatchOutcome, QuoteContext, dispatch_common_content,
};
use crate::proto::content_v1::{AttachmentPointer, Content};
use crate::services::AttachmentService;

use super::{AnonDmError, AnonDmService};

impl AnonInboxBackend for AnonDmService {
    type Id = AnonDmId;

    fn conversation_type(&self) -> ConversationType {
        ConversationType::AnonDm
    }
    fn label(&self) -> &'static str {
        "anon DM"
    }

    fn is_sender_blocked(&self, sender_id: &UserId) -> bool {
        self.is_user_blocked(sender_id).unwrap_or(false)
    }

    fn is_conversation_blocked(&self, id: &Self::Id) -> bool {
        AnonDmService::is_conversation_blocked(self, id).unwrap_or(false)
    }

    fn create_attachment_records(&self, pointers: &[AttachmentPointer]) -> Vec<AttachmentId> {
        let svc = AttachmentService::new_local(self.attachment_repo.clone());
        svc.create_from_pointers(pointers)
    }

    fn store_data_message(
        &self,
        id: &Self::Id,
        sender: &UserId,
        text: &str,
        attachments: &[AttachmentId],
        sent_at: u64,
        quote: Option<QuoteContext>,
    ) -> Result<String, String> {
        self.store_message_with_attachments(id, sender, text, attachments, sent_at, quote)
            .map(|m| m.as_str().to_string())
            .map_err(|e| e.to_string())
    }

    fn delete_message(&self, id: &Self::Id, sender: &UserId, ts: u64) -> Result<bool, String> {
        self.delete_message_by_timestamp(id, sender, ts)
            .map_err(|e| e.to_string())
    }

    fn apply_edit(
        &self,
        id: &Self::Id,
        sender: &UserId,
        target_ts: u64,
        text: &str,
        edit_ts: u64,
    ) -> Result<Option<crate::domain::message::Message>, String> {
        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(
            self.settings_repo.database().clone(),
        )
        .apply_received_edit(
            &self.message_repo,
            sender,
            Some((*id).into()),
            target_ts,
            text,
            edit_ts,
            0,
        )
        .map_err(|e| e.to_string())
    }

    fn add_reaction(
        &self,
        id: &Self::Id,
        target_sender: &UserId,
        target_ts_ns: u64,
        reactor: &UserId,
        emoji: &str,
    ) -> Result<bool, String> {
        AnonDmService::add_reaction(self, id, target_sender, target_ts_ns, reactor, emoji)
            .map_err(|e| e.to_string())
    }

    fn remove_reaction(
        &self,
        id: &Self::Id,
        target_sender: &UserId,
        target_ts_ns: u64,
        reactor: &UserId,
        emoji: &str,
    ) -> Result<bool, String> {
        AnonDmService::remove_reaction(self, id, target_sender, target_ts_ns, reactor, emoji)
            .map_err(|e| e.to_string())
    }

    fn receive_reaction(
        &self,
        id: &Self::Id,
        author: &UserId,
        target_ts: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, crate::error::StorageError> {
        let scope =
            crate::domain::reaction::ReactionScope::new((*id).into(), ConversationType::AnonDm);
        self.message_repo
            .hold_received_reaction(&scope, author, target_ts, reactor, emoji, remove, action_ts)?;
        self.message_repo
            .apply_held_reaction_scope(&scope, author, target_ts)
    }

    fn set_pinned(&self, id: &Self::Id, ts: u64, pinned: bool) -> Result<(), String> {
        self.set_message_pinned(id, ts, pinned)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn get_message_id_by_timestamp(&self, id: &Self::Id, ts: u64) -> Option<MessageId> {
        self.get_message_by_timestamp(id, ts)
            .ok()
            .flatten()
            .map(|m| m.id)
    }

    fn persist_delivery_status(&self, id: &Self::Id, timestamps: &[u64], status: &str) {
        let _ = self.update_delivery_status(id, timestamps, status);
    }
}

impl AnonDmService {
    /// Process an incoming message from an anonymous DM conversation.
    ///
    /// Delegates common content types (DataMessage, Typing, Reaction, Receipt,
    /// Pin, Edit) to `dispatch_common_content`. Handles DM-specific content
    /// (presence, profile updates, anon-group forwarding, calls) locally.
    pub fn process_incoming_content(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        inner_content: &Content,
    ) -> Result<Vec<DomainEvent>, AnonDmError> {
        use crate::proto::content_v1::content::Content as ContentType;

        // Verify conversation exists and sender matches peer (DM-specific gate).
        // Block lists are checked inside dispatch_common_content via the backend.
        match self.get_conversation(conversation_id) {
            Ok(Some(dm)) => {
                if dm.peer_id != *sender_id {
                    tracing::info!(
                        sender_id = %sender_id,
                        conversation_id = %conversation_id,
                        expected_peer_id = %dm.peer_id,
                        "[ANON_DM] Dropped message from unexpected sender"
                    );
                    return Ok(vec![]);
                }
                if !dm.is_ready() {
                    tracing::info!(
                        conversation_id = %conversation_id,
                        status = ?dm.status,
                        "[ANON_DM] Dropped message in non-active conversation"
                    );
                    return Ok(vec![]);
                }
            }
            Ok(None) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    "[ANON_DM] Dropped message for unknown conversation"
                );
                return Ok(vec![]);
            }
            Err(e) => {
                tracing::warn!(conversation_id = %conversation_id, error = %e, "[ANON_DM] Failed to check conversation");
                return Ok(vec![]);
            }
        }

        // DM deletes allow both participants; the common group dispatcher
        // intentionally remains author-only. Emit the stored target's identity.
        if let Some(ContentType::DataMessage(data)) = &inner_content.content {
            if let Some(delete) = &data.delete {
                if !data.channel_id.is_empty() {
                    return Ok(vec![]);
                }
                return Ok(self
                    .apply_targeted_received_delete(
                        conversation_id,
                        sender_id,
                        delete.target_author_aci.as_deref(),
                        delete.target_sent_timestamp,
                    )?
                    .into_iter()
                    .map(|message| DomainEvent::MessageDeleted {
                        message_id: message.id,
                        author_id: message.sender_id.unwrap_or(*sender_id),
                        channel_id: Some(message.conversation_id),
                        target_sent_timestamp: delete.target_sent_timestamp,
                    })
                    .collect());
            }
        }

        // Common content types (identical between DM and Group).
        match dispatch_common_content(conversation_id, sender_id, inner_content, self) {
            CommonDispatchOutcome::Handled(mut events) => {
                // A pending delete suppresses storage; do not reconstruct a
                // phantom live bubble from the successful insert call.
                events.retain(|event| match event {
                    DomainEvent::MessageReceived { message } => {
                        self.message_repo.get(&message.id).ok().flatten().is_some()
                    }
                    _ => true,
                });
                return Ok(events);
            }
            CommonDispatchOutcome::Dropped => return Ok(vec![]),
            CommonDispatchOutcome::NotHandled => {}
            CommonDispatchOutcome::Failed(error) => return Err(error.into()),
        }

        // DM-specific content types.
        let mut events = Vec::new();
        match &inner_content.content {
            Some(ContentType::AnonDmPresence(presence)) => {
                use crate::proto::content_v1::anon_dm_presence;

                let domain_presence = match anon_dm_presence::Status::try_from(presence.status).ok()
                {
                    Some(anon_dm_presence::Status::Online) => {
                        crate::domain::user::PresenceStatus::Online
                    }
                    Some(anon_dm_presence::Status::Away) => {
                        crate::domain::user::PresenceStatus::Away
                    }
                    Some(anon_dm_presence::Status::DoNotDisturb) => {
                        crate::domain::user::PresenceStatus::DoNotDisturb
                    }
                    Some(anon_dm_presence::Status::Offline) | _ => {
                        crate::domain::user::PresenceStatus::Offline
                    }
                };

                events.push(DomainEvent::UserPresenceChanged {
                    user_id: *sender_id,
                    presence: domain_presence,
                    timestamp: presence.timestamp,
                });
            }
            Some(ContentType::AnonDmProfileUpdate(profile_update)) => {
                use crate::crypto::profile_crypto;

                // Validate sender is part of the conversation (double-check already done above but keep for consistency)
                if let Ok(Some(mut conv)) = self.anon_dm_repo.get_conversation(conversation_id) {
                    let mut decrypted_avatar = None;
                    let profile_key: Option<[u8; 32]> =
                        profile_update.profile_key.as_ref().and_then(|pk| {
                            if pk.len() == 32 {
                                Some(pk.as_slice().try_into().unwrap())
                            } else {
                                None
                            }
                        });

                    // The name only ever arrives under the peer's profile key.
                    let display_name = profile_key
                        .zip(profile_update.encrypted_display_name.as_ref())
                        .and_then(|(pk, enc)| profile_crypto::decrypt_display_name(&pk, enc).ok())
                        .filter(|n| !n.is_empty());
                    if let Some(name) = &display_name {
                        conv.peer_display_name = Some(name.clone());
                    }

                    if let (Some(pk), Some(encrypted)) =
                        (&profile_key, &profile_update.encrypted_avatar)
                    {
                        if !encrypted.is_empty() {
                            match profile_crypto::decrypt_avatar(pk, encrypted) {
                                Ok(avatar) => {
                                    tracing::info!(
                                        avatar_size = avatar.len(),
                                        "Decrypted peer avatar from profile update"
                                    );
                                    decrypted_avatar = Some(avatar.clone());
                                    conv.peer_avatar = Some(avatar);
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        "Failed to decrypt avatar in profile update: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }

                    // Store the updated conversation
                    if let Err(e) = self.anon_dm_repo.update_conversation(&conv) {
                        tracing::warn!(
                            "Failed to update anon DM conversation with new profile: {}",
                            e
                        );
                    }

                    // Emit event for UI update
                    events.push(DomainEvent::AnonDmProfileUpdated {
                        conversation_id: *conversation_id,
                        user_id: *sender_id,
                        display_name,
                        avatar: decrypted_avatar,
                        avatar_url: profile_update.avatar_url.clone(),
                        bio: profile_update.bio.clone(),
                        timestamp: profile_update.timestamp,
                    });
                }
            }
            Some(ContentType::AnonGroupInvite(invite)) => {
                use crate::crypto::profile_crypto;

                let group_id =
                    crate::domain::anon_group::AnonGroupId::try_from_slice(&invite.group_id)
                        .unwrap_or_default();
                let mut pk = [0u8; 32];
                if invite.inviter_mailbox_pk.len() == 32 {
                    pk.copy_from_slice(&invite.inviter_mailbox_pk);
                }

                // Decrypt profile if key is present
                let mut display_name = None;
                let mut avatar = None;
                let profile_key: Option<[u8; 32]> = invite.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 {
                        Some(pk.as_slice().try_into().unwrap())
                    } else {
                        None
                    }
                });

                if let Some(pk) = &profile_key {
                    if let Some(encrypted) = &invite.encrypted_display_name {
                        display_name = profile_crypto::decrypt_display_name(pk, encrypted).ok();
                    }
                    if let Some(encrypted) = &invite.encrypted_avatar {
                        avatar = profile_crypto::decrypt_avatar(pk, encrypted).ok();
                    }
                }

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    has_profile = profile_key.is_some(),
                    "Processing anonymous group invite in anon DM"
                );

                events.push(DomainEvent::AnonGroupInviteReceived {
                    group_id,
                    group_name: invite.group_name.clone(),
                    inviter_id: *sender_id,
                    inviter_mailbox_pk: pk,
                    profile_key,
                    inviter_display_name: display_name,
                    inviter_avatar: avatar,
                });
            }
            Some(ContentType::AnonGroupJoinAck(ack)) => {
                use crate::crypto::profile_crypto;

                let group_id =
                    crate::domain::anon_group::AnonGroupId::try_from_slice(&ack.group_id)
                        .unwrap_or_default();

                // Decrypt profile if key is present
                let mut display_name = None;
                let mut avatar = None;
                let profile_key: Option<[u8; 32]> = ack.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 {
                        Some(pk.as_slice().try_into().unwrap())
                    } else {
                        None
                    }
                });

                if let Some(pk) = &profile_key {
                    if let Some(encrypted) = &ack.encrypted_display_name {
                        display_name = profile_crypto::decrypt_display_name(pk, encrypted).ok();
                    }
                    if let Some(encrypted) = &ack.encrypted_avatar {
                        avatar = profile_crypto::decrypt_avatar(pk, encrypted).ok();
                    }
                }

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    has_profile = profile_key.is_some(),
                    "Processing anonymous group join ack in anon DM"
                );

                events.push(DomainEvent::AnonGroupMemberJoined {
                    group_id,
                    user_id: *sender_id,
                    profile_key,
                    display_name,
                    avatar,
                });
            }
            Some(ContentType::AnonGroupMemberList(list)) => {
                let group_id =
                    crate::domain::anon_group::AnonGroupId::try_from_slice(&list.group_id)
                        .unwrap_or_default();

                tracing::info!(
                    sender_id = %sender_id,
                    group_id = %group_id,
                    member_count = list.members.len(),
                    "Processing anonymous group member list in anon DM"
                );

                // Emit event for UI to update local state via AnonGroupService
                events.push(DomainEvent::AnonGroupMemberListReceived {
                    group_id,
                    members: list.members.clone(),
                });
            }
            Some(ContentType::AnonCallOffer(offer)) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    room_name = %offer.room_name,
                    call_type = ?offer.call_type,
                    "[ANON_DM_CALL] Received call offer"
                );

                // Emit event for UI to show incoming call
                events.push(DomainEvent::AnonCallOfferReceived {
                    conversation_id: *conversation_id,
                    sender_id: *sender_id,
                    call_room_id: offer.call_room_id.clone(),
                    room_name: offer.room_name.clone(),
                    call_type: offer.call_type,
                    caller_display_name: offer.caller_display_name.clone(),
                    call_encryption_key: offer.call_encryption_key.clone(),
                    timestamp: offer.timestamp,
                });
            }
            Some(ContentType::AnonCallAnswer(answer)) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    accepted = answer.accepted,
                    "[ANON_DM_CALL] Received call answer"
                );

                // Emit event for UI to handle call acceptance/rejection
                events.push(DomainEvent::AnonCallAnswerReceived {
                    conversation_id: *conversation_id,
                    sender_id: *sender_id,
                    call_room_id: answer.call_room_id.clone(),
                    accepted: answer.accepted,
                    callee_display_name: answer.callee_display_name.clone(),
                    timestamp: inner_content.timestamp,
                });
            }
            Some(ContentType::AnonCallHangup(hangup)) => {
                tracing::info!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    reason = ?hangup.reason,
                    "[ANON_DM_CALL] Received call hangup"
                );

                // Emit event for UI to end call
                events.push(DomainEvent::AnonCallHangupReceived {
                    conversation_id: *conversation_id,
                    sender_id: *sender_id,
                    call_room_id: hangup.call_room_id.clone(),
                    reason: hangup.reason,
                    timestamp: inner_content.timestamp,
                });
            }
            _ => {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    "[ANON_DM] Ignoring unhandled inner content type"
                );
            }
        }

        Ok(events)
    }

    /// Store a data message received from the peer (wire-format `content_type` + `content_json`).
    ///
    /// `content_json` is the `{"text": "..."}` envelope produced by the caller.
    /// `message_content_from_type_and_json` unwraps it before persisting so the
    /// row stores real `MessageContent` (not the raw JSON string).
    pub fn store_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
        quote: Option<crate::inbox::handlers::base_anon::QuoteContext>,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        let content = crate::services::anon_shared::message_content_from_type_and_json(
            content_type,
            content_json,
        );
        let message = crate::services::anon_shared::build_anon_message(
            channel_id,
            ConversationType::AnonDm,
            *sender_id,
            content,
            Vec::new(),
            sent_at,
            quote,
        );
        let message_id = message.id;
        self.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored message {} in conversation {} with sent_at={}",
            message_id,
            conversation_id,
            sent_at
        );
        Ok(AnonDmMessageId::from_string(message_id.to_string()))
    }

    /// Store a text message.
    pub fn store_text_message(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        sent_at: u64,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        let content =
            crate::services::anon_shared::message_content_from_text_and_attachments(text, &[]);
        let message = crate::services::anon_shared::build_anon_message(
            channel_id,
            ConversationType::AnonDm,
            *sender_id,
            content,
            Vec::new(),
            sent_at,
            None,
        );
        let message_id = message.id;
        self.message_repo.insert(&message)?;
        tracing::debug!(
            "Stored text message {} in conversation {} with sent_at={}",
            message_id,
            conversation_id,
            sent_at
        );
        Ok(AnonDmMessageId::from_string(message_id.to_string()))
    }

    /// Store a message with text and/or attachments.
    ///
    /// `quote` carries reply metadata (target message id, author, preview) extracted
    /// from the inbound Content proto. Persisting it on the row is what keeps reply
    /// bubbles visible after a channel reload on the receiver side.
    pub fn store_message_with_attachments(
        &self,
        conversation_id: &AnonDmId,
        sender_id: &UserId,
        text: &str,
        attachment_ids: &[crate::domain::AttachmentId],
        sent_at: u64,
        quote: Option<crate::inbox::handlers::base_anon::QuoteContext>,
    ) -> Result<AnonDmMessageId, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        let content = crate::services::anon_shared::message_content_from_text_and_attachments(
            text,
            attachment_ids,
        );
        let message = crate::services::anon_shared::build_anon_message(
            channel_id,
            ConversationType::AnonDm,
            *sender_id,
            content,
            attachment_ids.to_vec(),
            sent_at,
            quote,
        );
        let message_id = message.id;
        let has_text = !text.is_empty();

        self.message_repo.insert(&message)?;
        tracing::debug!(
            conversation_id = %conversation_id,
            message_id = %message_id,
            sent_at = sent_at,
            has_text = has_text,
            attachment_count = attachment_ids.len(),
            "[ANON_DM] store_message_with_attachments completed"
        );
        Ok(AnonDmMessageId::from_string(message_id.to_string()))
    }

    /// Update delivery status for messages by timestamps.
    pub fn update_delivery_status(
        &self,
        conversation_id: &AnonDmId,
        timestamps: &[u64],
        status: &str,
    ) -> Result<usize, AnonDmError> {
        let channel_id: ChannelId = (*conversation_id).into();
        Ok(self.message_repo.update_delivery_status_by_timestamps(
            &channel_id,
            &ConversationType::AnonDm,
            timestamps,
            status,
        )?)
    }
}
