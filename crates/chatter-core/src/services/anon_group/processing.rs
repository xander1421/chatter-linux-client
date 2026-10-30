//! Incoming content processing for anonymous groups.
//!
//! Mirrors `anon_dm::processing` — delegates common content types to
//! `dispatch_common_content` and handles group-specific variants (calls,
//! member list, leave, kick, profile share, update) locally.

use crate::domain::UserId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{AttachmentId, MessageId};
use crate::domain::message::ConversationType;
use crate::inbox::handlers::DomainEvent;
use crate::inbox::handlers::base_anon::{
    AnonInboxBackend, CommonDispatchOutcome, QuoteContext, dispatch_common_content,
};
use crate::proto::content_v1::{AttachmentPointer, Content};

use super::{AnonGroupError, AnonGroupService};

impl AnonInboxBackend for AnonGroupService {
    type Id = AnonGroupId;

    fn conversation_type(&self) -> ConversationType {
        ConversationType::AnonGroup
    }
    fn label(&self) -> &'static str {
        "anon group"
    }

    // Both block lists are keyed by group, so they are enforced at the top of
    // `process_incoming_content`, which has the group id; these trait hooks
    // (keyed by sender alone, or by id alone) stay at their defaults.

    fn create_attachment_records(
        &self,
        pointers: &[AttachmentPointer],
    ) -> Result<Vec<AttachmentId>, crate::error::StorageError> {
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

    fn has_data_message(
        &self,
        id: &Self::Id,
        sender: &UserId,
        sent_at: u64,
    ) -> Result<bool, crate::error::StorageError> {
        let channel: crate::domain::ChannelId = (*id).into();
        let message_id = MessageId::from_secret_sender_timestamp(&channel, sender, sent_at);
        Ok(self
            .message_service
            .message_repo
            .get(&message_id)?
            .is_some_and(|message| {
                message.conversation_id == channel && message.sender_id == Some(*sender)
            }))
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
        self.message_service
            .apply_channel_edit(&(*id).into(), sender, target_ts, text, edit_ts, 0)
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
        AnonGroupService::add_reaction(self, id, target_sender, target_ts_ns, reactor, emoji)
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
        AnonGroupService::remove_reaction(self, id, target_sender, target_ts_ns, reactor, emoji)
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
            crate::domain::reaction::ReactionScope::new((*id).into(), ConversationType::AnonGroup);
        self.message_service
            .message_repo
            .hold_received_reaction(&scope, author, target_ts, reactor, emoji, remove, action_ts)?;
        self.message_service
            .message_repo
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

    fn persist_delivery_status(
        &self,
        id: &Self::Id,
        timestamps: &[u64],
        status: &str,
    ) -> Result<(), String> {
        self.message_service
            .message_repo
            .update_delivery_status_by_timestamps(
                &(*id).into(),
                &ConversationType::AnonGroup,
                timestamps,
                status,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Surface persisted group receipts after the authenticated roster gate.
    fn emit_receipts(&self) -> bool {
        true
    }
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

        // Block lists, fail closed: a store error counts as blocked.
        // A blocked group is ignored entirely. A member blocked in this group
        // is muted — posts, reactions, typing, calls, profile shares dropped —
        // but the group's structure (leave, kick, member list, update) still
        // applies, so the roster stays true and we never keep sending to
        // someone who left.
        let group_blocked = match self.anon_group_repo.get_group(group_id) {
            Ok(group) => group.is_some_and(|g| !g.status.can_receive()),
            Err(_) => true,
        };
        if group_blocked {
            tracing::debug!("Dropped content for blocked group");
            return Ok(vec![]);
        }
        let sender_blocked = self
            .anon_group_repo
            .is_user_blocked(group_id, peer_id)
            .unwrap_or(true);
        let structural = matches!(
            inner_content.content,
            Some(ContentType::AnonGroupLeave(_))
                | Some(ContentType::AnonGroupKick(_))
                | Some(ContentType::AnonGroupMemberList(_))
                | Some(ContentType::AnonGroupUpdate(_))
        );
        if sender_blocked && !structural {
            tracing::debug!("Dropped content from member blocked in group");
            return Ok(vec![]);
        }

        // Roster gate, fail closed (a store error reads as "not a member").
        // Structural messages authorize themselves (owner signature, sender
        // == leaver). Content this build does not recognise is dropped: there
        // is nothing to deliver later. Anything else waits in `held_posts` for
        // a member list that adds the sender.
        if !structural && !self.sender_in_roster(group_id, peer_id) {
            let held = inner_content.content.is_some();
            if held {
                let now = std::time::Instant::now();
                self.held_posts.hold(
                    *group_id,
                    super::held::HeldPost {
                        at: now,
                        sender: *peer_id,
                        content: inner_content.clone(),
                    },
                    now,
                );
            }
            tracing::debug!(
                held,
                "Content from a sender outside the roster not delivered"
            );
            return Ok(vec![]);
        }

        // Common content types first.
        match dispatch_common_content(group_id, peer_id, inner_content, self.as_ref()) {
            CommonDispatchOutcome::Handled(events) => return Ok(events),
            CommonDispatchOutcome::Dropped => return Ok(vec![]),
            CommonDispatchOutcome::NotHandled => {}
            CommonDispatchOutcome::Failed(error) => return Err(error.into()),
        }

        // Group-specific content types.
        let mut events = Vec::new();
        match &inner_content.content {
            Some(ContentType::AnonGroupMemberList(list)) => {
                // The first roster bootstraps a new joiner's history. Later
                // rosters can announce newly confirmed members to existing peers.
                let had_roster = self.anon_group_repo.has_applied_roster(group_id)?;
                let confirmed_before: std::collections::HashSet<UserId> = self
                    .get_members(group_id)?
                    .into_iter()
                    .filter(|member| member.mailbox_pk.is_some())
                    .map(|member| member.user_id)
                    .collect();
                // Sender authorization (owner only) lives in handle_member_list,
                // the one function every transport routes through.
                let members = list.members.clone();
                if let Err(e) = self
                    .handle_member_list_ordered(
                        *group_id,
                        *peer_id,
                        members.clone(),
                        list.timestamp,
                    )
                    .and_then(|applied| {
                        if applied {
                            Ok(())
                        } else {
                            Err(AnonGroupError::InvalidData("Stale roster".into()))
                        }
                    })
                {
                    tracing::warn!( error = %e, "Failed to handle member list");
                    return Ok(events);
                }

                events.push(DomainEvent::AnonGroupMemberListReceived {
                    group_id: *group_id,
                    members,
                });
                if had_roster {
                    for member in self.get_members(group_id)? {
                        if member.mailbox_pk.is_some()
                            && !confirmed_before.contains(&member.user_id)
                        {
                            events.push(DomainEvent::AnonGroupMemberJoined {
                                group_id: *group_id,
                                user_id: member.user_id,
                                profile_key: member.profile_key,
                                display_name: member.display_name,
                                avatar: member.avatar,
                            });
                        }
                    }
                }
                // Posts that raced this list in are delivered now.
                events.extend(self.release_held_posts(group_id));

                tracing::info!("Processed group member list");
            }
            Some(ContentType::AnonGroupLeave(leave)) => {
                match self.handle_leave_ordered(*group_id, *peer_id, leave.timestamp) {
                    Ok(true) => {
                        events.push(DomainEvent::AnonGroupMemberLeft {
                            group_id: *group_id,
                            user_id: *peer_id,
                        });
                        tracing::info!("Member left group");
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, "Failed to handle group leave"),
                }
            }
            Some(ContentType::AnonGroupKick(kick)) => {
                let kicked_user_id = match UserId::try_from_slice(&kick.kicked_user_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid kicked_user_id in AnonGroupKick");
                        return Ok(events);
                    }
                };
                // Announced only once verified and applied: a refused kick
                // (unsigned, forged, from a non-admin, at the owner) must
                // not show up as "X removed by Y".
                match self.handle_kick(
                    *group_id,
                    kicked_user_id,
                    *peer_id,
                    &kick.admin_signature,
                    kick.timestamp,
                ) {
                    Ok(true) => {
                        events.push(DomainEvent::AnonGroupMemberKicked {
                            group_id: *group_id,
                            user_id: kicked_user_id,
                            kicked_by: *peer_id,
                        });
                        tracing::info!("Member kicked");
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, "Failed to handle group kick"),
                }
            }
            Some(ContentType::AnonGroupProfileShare(profile)) => {
                let profile_key = profile.profile_key.as_ref().and_then(|pk| {
                    if pk.len() == 32 {
                        let mut k = [0u8; 32];
                        k.copy_from_slice(pk);
                        Some(k)
                    } else {
                        None
                    }
                });

                use crate::crypto::profile_crypto::decrypt_display_name;

                let display_name = match (&profile_key, &profile.encrypted_display_name) {
                    (Some(pk), Some(enc)) if !enc.is_empty() => {
                        decrypt_display_name(pk, enc).ok().filter(|n| !n.is_empty())
                    }
                    _ => None,
                };
                let avatar = profile.avatar.clone();

                let svc = self.clone();
                let group_id_owned = *group_id;
                let peer_id_owned = *peer_id;
                tokio::spawn(async move {
                    let current = svc
                        .anon_group_repo
                        .get_member(&group_id_owned, &peer_id_owned)
                        .ok()
                        .flatten()
                        .and_then(|m| m.avatar);
                    let avatar = svc.resolve_member_avatar(avatar.as_ref(), current).await;
                    if let Err(e) = svc
                        .handle_profile_share(
                            group_id_owned,
                            peer_id_owned,
                            profile_key,
                            display_name,
                            avatar,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to handle profile share");
                    }
                });

                tracing::debug!("Processed profile share");
            }
            Some(ContentType::AnonGroupUpdate(update)) => {
                // Applied here too: updates (renames, roles, the ownership
                // hand-off) ride the group mailbox, not only DMs. Announced
                // only when authorized and applied.
                match self.handle_update(*group_id, *peer_id, update) {
                    Ok(true) => {
                        events.push(DomainEvent::AnonGroupUpdated {
                            group_id: *group_id,
                            update: update.clone(),
                            updated_by: *peer_id,
                        });
                        tracing::info!("Group updated");
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!( error = %e, "Failed to handle group update")
                    }
                }
            }
            _ => {
                tracing::debug!("Ignoring unhandled inner content type");
            }
        }

        Ok(events)
    }
}

impl AnonGroupService {
    /// Whether `peer` may post in `group`: us, or a member of its roster.
    /// Fail closed: a store error is "no".
    fn sender_in_roster(&self, group: &AnonGroupId, peer: &UserId) -> bool {
        self.current_user_id().is_ok_and(|me| me == *peer)
            || self.anon_group_repo.is_member(group, peer).unwrap_or(false)
    }

    /// Re-check every post held for `group` against its roster now: deliver
    /// the ones whose sender is in it, keep the rest until they expire.
    /// Returns the delivered posts' events.
    pub fn release_held_posts(
        self: &std::sync::Arc<Self>,
        group: &AnonGroupId,
    ) -> Vec<DomainEvent> {
        let now = std::time::Instant::now();
        let mut events = Vec::new();
        for post in self.held_posts.take(group, now) {
            if self.sender_in_roster(group, &post.sender) {
                match self.process_incoming_content(group, &post.sender, &post.content) {
                    Ok(delivered) => events.extend(delivered),
                    Err(e) => {
                        tracing::warn!( error = %e, "Held post failed on release")
                    }
                }
            } else {
                self.held_posts.hold(*group, post, now);
            }
        }
        events
    }
}
