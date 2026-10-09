//! anon_group façade — thin delegates to services.

use super::ChatterClient;

impl ChatterClient {
    // ── Anon group façade ────────────────────────────────────────────────

    pub fn list_anon_groups(
        &self,
    ) -> Result<
        Vec<crate::domain::anon_group::AnonGroup>,
        crate::services::anon_group::AnonGroupError,
    > {
        self.anon_groups().list_groups()
    }

    /// Anon groups to list loose in the sidebar: anything filed into a custom
    /// section is excluded, because it renders inside that folder instead — it
    /// used to render in BOTH. Same rule `sidebar_channels` and the DM sidebar
    /// already apply; private groups were the one list that never got it, so a
    /// foldered group showed twice and stayed doubled across restarts.
    pub fn sidebar_private_groups(&self) -> Vec<crate::domain::anon_group::AnonGroup> {
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        self.list_anon_groups()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| !sectioned.contains(&g.id.to_string()))
            .collect()
    }

    pub fn get_anon_group(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Option<crate::domain::anon_group::AnonGroup> {
        self.anon_groups().get_group(group_id).ok().flatten()
    }

    pub fn get_anon_group_members(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Vec<crate::domain::anon_group::AnonGroupMember> {
        self.anon_groups().get_members(group_id).unwrap_or_default()
    }

    /// Whether to offer Leave for a private group: not to its owner while
    /// others remain (they must make someone else owner first).
    pub fn can_leave_anon_group(&self, group_id: &crate::domain::anon_group::AnonGroupId) -> bool {
        self.anon_groups().can_leave(group_id)
    }

    /// Members blocked within `group_id` on this device.
    pub fn anon_group_blocked_members(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Vec<crate::domain::ids::UserId> {
        self.anon_groups()
            .get_blocked_users(group_id)
            .unwrap_or_default()
    }

    pub fn get_anon_group_member_display_name(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<String> {
        self.anon_groups()
            .get_member_display_name(group_id, user_id)
            .ok()
            .flatten()
    }

    /// Build the synthetic system `Message` shown for an anon-group membership
    /// change ("X joined", "X left", "X removed by Y"). The membership events
    /// arrive as bare metadata; constructing the domain `Message` is core's
    /// job, not the UI's.
    pub fn anon_group_system_message(
        &self,
        group_id: crate::domain::anon_group::AnonGroupId,
        actor_id: crate::domain::ids::UserId,
        content: crate::domain::message::SystemMessage,
    ) -> crate::domain::message::Message {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, Message, MessageContent};
        let now = chrono::Utc::now();
        Message {
            id: MessageId::new(),
            conversation_id: group_id.into(),
            conversation_type: ConversationType::AnonGroup,
            sender_id: Some(actor_id),
            device_id: None,
            content: MessageContent::System(content),
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
            is_read: true,
            sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
            created_at: now,
            updated_at: now,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
            failure_reason: None,
        }
    }

    pub fn get_latest_anon_group_message(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Option<crate::domain::message::Message> {
        self.anon_groups()
            .get_latest_message(group_id)
            .ok()
            .flatten()
    }

    pub fn get_anon_group_messages_asc(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        before: Option<&chrono::DateTime<chrono::Utc>>,
        limit: i32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::anon_group::AnonGroupError>
    {
        let mut messages = self
            .anon_groups()
            .get_messages_asc(group_id, before, limit)?;
        // Join reaction counts here (keyed by sent_at) so callers render a
        // complete Message. Previously the UI issued a second call and merged —
        // duplicated across sidebar sites and a data-shaping leak into the view.
        let reactions = self
            .anon_groups()
            .get_all_reaction_counts(group_id)
            .unwrap_or_default();
        for msg in &mut messages {
            if let Some(counts) = reactions.get(&msg.sent_at) {
                msg.reaction_counts = counts
                    .iter()
                    .map(|(emoji, count)| crate::domain::message::ReactionCount {
                        emoji: emoji.clone(),
                        count: *count,
                    })
                    .collect();
            }
        }
        Ok(messages)
    }

    pub fn list_anon_group_pinned(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Vec<crate::domain::message::Message> {
        self.anon_groups().list_pinned(group_id).unwrap_or_default()
    }

    pub async fn refresh_expiring_group_mailboxes(
        &self,
    ) -> Result<
        Vec<crate::domain::anon_group::AnonGroupId>,
        crate::services::anon_group::AnonGroupError,
    > {
        self.anon_groups().refresh_expiring_group_mailboxes().await
    }

    /// Who can be invited to a private (anon) group: accepted friends and
    /// peers of ready anon DMs, de-duplicated, never this user, and — for an
    /// existing `group` — include live pending invitations as explicit retries.
    /// Confirmed members stay excluded. Read from local state,
    /// so there is no page cap on the friend list.
    ///
    /// In Shield mode friends are not offered — same rule as channel invites:
    /// only peers known anonymously (a friend who is also an anon-DM peer is
    /// still listed, as that anon peer).
    pub fn list_anon_group_invite_candidates(
        &self,
        group: Option<&crate::domain::anon_group::AnonGroupId>,
    ) -> Vec<AnonGroupInviteCandidate> {
        use crate::domain::ids::UserId;
        use std::collections::HashSet;

        let me = self.auth_state().get().current_user_id;
        let mut seen: HashSet<UserId> = me.into_iter().collect();
        let mut retries = HashSet::new();
        if let Some(group) = group {
            for member in self.get_anon_group_members(group) {
                if Some(member.user_id) != me
                    && self
                        .anon_groups()
                        .can_retry_pending_invitation(group, &member.user_id)
                        .unwrap_or(false)
                {
                    retries.insert(member.user_id);
                } else {
                    seen.insert(member.user_id);
                }
            }
        }

        let mut out = Vec::new();
        if !self.anon_mode_enabled() {
            for peer in self.listed_friends() {
                if seen.insert(peer) {
                    out.push(AnonGroupInviteCandidate {
                        user_id: peer,
                        display_name: self.display_name(&peer),
                        is_anonymous: false,
                    });
                }
            }
        }
        for dm in self.list_anon_dm_ready_conversations() {
            if seen.insert(dm.peer_id) {
                out.push(AnonGroupInviteCandidate {
                    user_id: dm.peer_id,
                    display_name: dm
                        .peer_display_name
                        .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id)),
                    is_anonymous: true,
                });
            }
        }
        for candidate in &mut out {
            if retries.contains(&candidate.user_id) {
                candidate.display_name = format!("{} — Retry invitation", candidate.display_name);
            }
        }
        out
    }

    pub async fn broadcast_profile_to_owned_groups(
        &self,
    ) -> Result<(), crate::services::anon_group::AnonGroupError> {
        self.anon_groups().broadcast_profile_to_owned_groups().await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn accept_anon_group_invite(
        &self,
        group_id: crate::domain::anon_group::AnonGroupId,
        group_name: String,
        owner_id: crate::domain::ids::UserId,
        inviter_id: crate::domain::ids::UserId,
        inviter_mailbox_pk: [u8; 32],
        inviter_profile_key: Option<[u8; 32]>,
        inviter_display_name: Option<String>,
        inviter_avatar: Option<Vec<u8>>,
    ) -> Result<crate::domain::anon_group::AnonGroup, crate::services::anon_group::AnonGroupError>
    {
        self.anon_groups()
            .accept_invite(
                group_id,
                group_name,
                owner_id,
                inviter_id,
                inviter_mailbox_pk,
                inviter_profile_key,
                inviter_display_name,
                inviter_avatar,
            )
            .await
    }
}

/// A row of the private-group invite picker; see
/// [`ChatterClient::list_anon_group_invite_candidates`].
#[derive(Debug, Clone)]
pub struct AnonGroupInviteCandidate {
    pub user_id: crate::domain::ids::UserId,
    pub display_name: String,
    /// Reached through an anon DM rather than a server friendship.
    pub is_anonymous: bool,
}
