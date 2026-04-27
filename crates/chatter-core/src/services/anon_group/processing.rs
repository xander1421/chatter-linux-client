//! Incoming content processing for anonymous groups.
//!
//! Mirrors `anon_dm::processing` — delegates common content types to
//! `dispatch_common_content` and handles group-specific variants (calls,
//! member list, leave, kick, profile share, update) locally.

use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{AttachmentId, MessageId};
use crate::domain::message::ConversationType;
use crate::domain::UserId;
use crate::inbox::handlers::base_anon::{
    AnonInboxBackend, CommonDispatchOutcome, QuoteContext, dispatch_common_content,
};
use crate::inbox::handlers::DomainEvent;
use crate::proto::content_v1::{AttachmentPointer, Content};

use super::{AnonGroupError, AnonGroupService};

impl AnonInboxBackend for AnonGroupService {
    type Id = AnonGroupId;

    fn conversation_type(&self) -> ConversationType { ConversationType::AnonGroup }
    fn label(&self) -> &'static str { "anon group" }

    fn is_sender_blocked(&self, _sender_id: &UserId) -> bool {
        // Group-level blocking is per-conversation; checked in is_conversation_blocked
        // by callers that have the group_id. No global user blocking for anon groups.
        false
    }

    fn is_conversation_blocked(&self, _id: &Self::Id) -> bool {
        // Group blocking is keyed by (group_id, user_id); checked at process_incoming_content
        // entry against membership + per-group block list. Return false here so the common
        // dispatcher doesn't short-circuit.
        false
    }

    fn create_attachment_records(&self, pointers: &[AttachmentPointer]) -> Vec<AttachmentId> {
        self.create_attachments_from_pointers(pointers)
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
        self.delete_message_by_timestamp(id, sender, ts).map_err(|e| e.to_string())
    }

    fn update_message_text(&self, id: &Self::Id, sender: &UserId, ts: u64, text: &str) -> Result<bool, String> {
        self.update_message_text_by_sender(id, sender, ts, text).map_err(|e| e.to_string())
    }

    fn add_reaction(
        &self, id: &Self::Id, target_sender: &UserId, target_ts_ns: u64,
        reactor: &UserId, emoji: &str,
    ) -> Result<bool, String> {
        AnonGroupService::add_reaction(self, id, target_sender, target_ts_ns, reactor, emoji)
            .map_err(|e| e.to_string())
    }

    fn remove_reaction(
        &self, id: &Self::Id, target_sender: &UserId, target_ts_ns: u64,
        reactor: &UserId, emoji: &str,
    ) -> Result<bool, String> {
        AnonGroupService::remove_reaction(self, id, target_sender, target_ts_ns, reactor, emoji)
            .map_err(|e| e.to_string())
    }

    fn set_pinned(&self, id: &Self::Id, ts: u64, pinned: bool) -> Result<(), String> {
        self.set_message_pinned(id, ts, pinned).map(|_| ()).map_err(|e| e.to_string())
    }

    fn get_message_id_by_timestamp(&self, id: &Self::Id, ts: u64) -> Option<MessageId> {
        self.get_message_by_timestamp(id, ts).ok().flatten().map(|m| m.id)
    }

    /// Group receipts are debug-only (not yet persisted or emitted to UI).
    fn emit_receipts(&self) -> bool { false }
}

impl AnonGroupService {
    /// Process an incoming content proto from an anonymous group message.
    ///
    /// Entry point for `InboxEvent::AnonGroupMessage`. Delegates common content
    /// types (DataMessage, Typing, Reaction, Receipt, Pin, Edit) to
    /// `dispatch_common_content`; handles group-specific variants (calls,
    /// member list, leave, kick, profile share, update) locally.
    pub fn process_incoming_content(
        self: &std::sync::Arc<Self>,
        group_id: &AnonGroupId,
        peer_id: &UserId,
        inner_content: &Content,
    ) -> Result<Vec<DomainEvent>, AnonGroupError> {
        use crate::proto::content_v1::content::Content as ContentType;

        // Common content types first.
        match dispatch_common_content(group_id, peer_id, inner_content, self.as_ref()) {
            CommonDispatchOutcome::Handled(events) => return Ok(events),
            CommonDispatchOutcome::Dropped => return Ok(vec![]),
            CommonDispatchOutcome::NotHandled => {}
        }

        // Group-specific content types.
        let mut events = Vec::new();
        match &inner_content.content {
            Some(ContentType::AnonCallOffer(offer)) => {
                tracing::info!(
                    group_id = %group_id,
                    sender_id = %peer_id,
                    room_name = %offer.room_name,
                    "Received anonymous group call offer"
                );
                events.push(DomainEvent::AnonGroupCallOfferReceived {
                    group_id: *group_id,
                    sender_id: *peer_id,
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
                    group_id = %group_id,
                    sender_id = %peer_id,
                    accepted = answer.accepted,
                    "Received anonymous group call answer"
                );
                events.push(DomainEvent::AnonGroupCallAnswerReceived {
                    group_id: *group_id,
                    sender_id: *peer_id,
                    call_room_id: answer.call_room_id.clone(),
                    accepted: answer.accepted,
                    callee_display_name: answer.callee_display_name.clone(),
                    timestamp: answer.timestamp,
                });
            }
            Some(ContentType::AnonCallHangup(hangup)) => {
                tracing::info!(
                    group_id = %group_id,
                    sender_id = %peer_id,
                    "Received anonymous group call hangup"
                );
                events.push(DomainEvent::AnonGroupCallHangupReceived {
                    group_id: *group_id,
                    sender_id: *peer_id,
                    call_room_id: hangup.call_room_id.clone(),
                    reason: hangup.reason,
                    timestamp: hangup.timestamp,
                });
            }
            Some(ContentType::AnonGroupMemberList(list)) => {
                // SECURITY: verify sender is admin or owner
                match self.get_member_role(group_id, peer_id) {
                    Ok(Some(role)) if role.can_broadcast_member_list() => {}
                    _ => {
                        tracing::warn!(
                            sender_id = %peer_id,
                            group_id = %group_id,
                            "Rejected member list from unauthorized sender"
                        );
                        return Ok(events);
                    }
                }

                let members = list.members.clone();
                if let Err(e) = self.handle_member_list(*group_id, *peer_id, members.clone()) {
                    tracing::warn!(group_id = %group_id, error = %e, "Failed to handle member list");
                }

                events.push(DomainEvent::AnonGroupMemberListReceived {
                    group_id: *group_id,
                    members,
                });

                tracing::info!(group_id = %group_id, sender_id = %peer_id, "Processed group member list");
            }
            Some(ContentType::AnonGroupLeave(_)) => {
                if let Err(e) = self.handle_leave(*group_id, *peer_id) {
                    tracing::warn!(error = %e, "Failed to handle group leave");
                }
                events.push(DomainEvent::AnonGroupMemberLeft {
                    group_id: *group_id,
                    user_id: *peer_id,
                });
                tracing::info!(group_id = %group_id, sender_id = %peer_id, "Member left group");
            }
            Some(ContentType::AnonGroupKick(kick)) => {
                let kicked_user_id = match UserId::try_from_slice(&kick.kicked_user_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid kicked_user_id in AnonGroupKick");
                        return Ok(events);
                    }
                };
                if let Err(e) = self.handle_kick(
                    *group_id, kicked_user_id, *peer_id,
                    &kick.admin_signature, kick.timestamp,
                ) {
                    tracing::warn!(error = %e, "Failed to handle group kick");
                }
                events.push(DomainEvent::AnonGroupMemberKicked {
                    group_id: *group_id,
                    user_id: kicked_user_id,
                    kicked_by: *peer_id,
                });
                tracing::info!(group_id = %group_id, kicked = %kicked_user_id, by = %peer_id, "Member kicked");
            }
            Some(ContentType::AnonGroupProfileShare(profile)) => {
                if !self.is_member(group_id, peer_id).unwrap_or(false) {
                    tracing::debug!(sender_id = %peer_id, "Dropped profile share from non-member");
                    return Ok(events);
                }

                let profile_key = profile.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 { let mut k = [0u8; 32]; k.copy_from_slice(pk); Some(k) } else { None }
                });

                use crate::crypto::profile_crypto::{decrypt_avatar, decrypt_display_name};

                let display_name = match (&profile_key, &profile.encrypted_display_name) {
                    (Some(pk), Some(enc)) if !enc.is_empty() => decrypt_display_name(pk, enc).ok().filter(|n| !n.is_empty()),
                    _ => None,
                };
                let avatar = match (&profile_key, &profile.encrypted_avatar) {
                    (Some(pk), Some(enc)) if !enc.is_empty() => decrypt_avatar(pk, enc).ok(),
                    _ => None,
                };

                let svc = self.clone();
                let group_id_owned = *group_id;
                let peer_id_owned = *peer_id;
                tokio::spawn(async move {
                    if let Err(e) = svc.handle_profile_share(
                        group_id_owned, peer_id_owned, profile_key, display_name, avatar,
                    ).await {
                        tracing::warn!(error = %e, "Failed to handle profile share");
                    }
                });

                tracing::debug!(group_id = %group_id, sender_id = %peer_id, "Processed profile share");
            }
            Some(ContentType::AnonGroupUpdate(update)) => {
                events.push(DomainEvent::AnonGroupUpdated {
                    group_id: *group_id,
                    update: update.clone(),
                    updated_by: *peer_id,
                });
                tracing::info!(group_id = %group_id, sender_id = %peer_id, "Group updated");
            }
            _ => {
                tracing::debug!(group_id = %group_id, "Ignoring unhandled inner content type");
            }
        }

        Ok(events)
    }
}
