//! Conversation-scoped identities used to resolve message-body mentions.

use super::ChatterClient;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::mention::MentionMember;
use crate::domain::message::ConversationType;

impl ChatterClient {
    pub fn mention_members(
        &self,
        conversation_type: &ConversationType,
        conversation_id: &ChannelId,
    ) -> Vec<MentionMember> {
        let mut ids: Vec<UserId> = match conversation_type {
            ConversationType::Channel => self.list_channel_members_local(conversation_id)
                .into_iter().map(|member| member.user_id).collect(),
            ConversationType::AnonGroup => {
                if self.get_channel_local(conversation_id).is_some() {
                    self.list_channel_members_local(conversation_id)
                        .into_iter().map(|member| member.user_id).collect()
                } else {
                    let id = crate::domain::anon_group::AnonGroupId::from_uuid(*conversation_id.as_uuid());
                    self.get_anon_group_members(&id).into_iter().map(|member| member.user_id).collect()
                }
            }
            ConversationType::ServerDm => vec![UserId::from_uuid(*conversation_id.as_uuid())],
            ConversationType::AnonDm => {
                let id = crate::domain::anon_dm::AnonDmId::from_uuid(*conversation_id.as_uuid());
                self.get_anon_dm_conversation(&id).map(|dm| vec![dm.peer_id]).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        if let Some(me) = self.auth_snapshot().current_user_id {
            if !ids.contains(&me) { ids.push(me); }
        }
        ids.into_iter().map(|user_id| {
            let display = self.display_name_in(conversation_type, conversation_id, &user_id);
            let username = self.get_user_local(&user_id).map(|u| u.username).unwrap_or_default();
            let mut names = vec![display];
            if !username.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(&username)) {
                names.push(username);
            }
            MentionMember { user_id, names }
        }).collect()
    }
}
