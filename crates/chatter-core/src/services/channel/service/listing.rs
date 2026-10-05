//! Channel listing and archive operations.
//!
//! List, search, and archive methods for ChannelService.

use crate::domain::channel::{Channel, ChannelType};
use crate::domain::ids::ChannelId;
use crate::proto::chat_v1;
use crate::services::current_user::CurrentUserProvider;

use crate::services::channel::converters::{
    domain_channel_type_to_proto, proto_to_domain_channel, proto_to_domain_membership,
};
use crate::services::channel::error::ChannelError;

use super::ChannelService;

impl ChannelService {
    /// Archive a channel.
    pub async fn archive_channel(&self, channel_id: &ChannelId) -> Result<Channel, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let response = channel_client.archive_channel(&channel_id_str).await?;

        let proto_channel = response.archived_channel.ok_or_else(|| {
            ChannelError::InvalidInput("no archived_channel in response".to_string())
        })?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        self.channel_repo.upsert(&channel)?;

        Ok(channel)
    }

    /// Unarchive a channel.
    pub async fn unarchive_channel(&self, channel_id: &ChannelId) -> Result<Channel, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let response = channel_client.unarchive_channel(&channel_id_str).await?;

        let proto_channel = response.unarchived_channel.ok_or_else(|| {
            ChannelError::InvalidInput("no unarchived_channel in response".to_string())
        })?;

        let channel = proto_to_domain_channel(&proto_channel)?;

        self.channel_repo.upsert(&channel)?;

        Ok(channel)
    }

    /// List archived channel IDs.
    pub async fn list_archived_channels(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<ChannelId>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let mut channel_client = self.channel_client();
        let response = channel_client
            .list_archived_channels(limit as i32, offset as i32)
            .await?;

        let mut channel_ids = Vec::with_capacity(response.archived_channel_ids.len());
        for id_str in &response.archived_channel_ids {
            let channel_id: ChannelId = id_str.parse().map_err(|_| {
                ChannelError::InvalidInput(format!("invalid channel ID: {}", id_str))
            })?;
            channel_ids.push(channel_id);
        }

        Ok(channel_ids)
    }

    /// List channels with pagination.
    pub async fn list_channels(
        &self,
        limit: u32,
        offset: u32,
        archived: Option<bool>,
    ) -> Result<Vec<Channel>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let mut channel_client = self.channel_client();
        let response = channel_client
            .list_channels(limit as i32, offset as i32, archived)
            .await?;

        let mut channels = Vec::with_capacity(response.channels.len());
        for proto in &response.channels {
            let channel = proto_to_domain_channel(proto)?;

            self.channel_repo.upsert(&channel)?;

            // Sync memberships from the proto response
            for proto_member in &proto.members {
                match proto_to_domain_membership(&channel.id, proto_member) {
                    Ok(membership) => {
                        if let Err(e) = self.channel_repo.insert_or_ignore_membership(&membership) {
                            tracing::warn!(
                                channel_id = %channel.id,
                                user_id = %membership.user_id,
                                error = %e,
                                "Failed to sync membership"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel.id,
                            error = %e,
                            "Failed to convert proto membership"
                        );
                    }
                }
            }

            channels.push(channel);
        }

        Ok(channels)
    }

    /// Search for channels by query.
    pub async fn search_channels(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
        type_filter: Vec<ChannelType>,
    ) -> Result<Vec<Channel>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let proto_type_filter: Vec<chat_v1::ChannelType> = type_filter
            .into_iter()
            .map(domain_channel_type_to_proto)
            .collect();

        let mut channel_client = self.channel_client();
        let response = channel_client
            .search_channels(
                query,
                limit as i32,
                offset as i32,
                vec![],
                proto_type_filter,
            )
            .await?;

        let mut channels = Vec::with_capacity(response.results.len());
        for proto in &response.results {
            let channel = proto_to_domain_channel(proto)?;
            channels.push(channel);
        }

        Ok(channels)
    }

    /// Get all channels the current user is a member of (local query).
    pub fn get_channels_for_user(&self) -> Result<Vec<Channel>, ChannelError> {
        let current_user = self.settings_repo.current_user_id()?;
        let channels = self.channel_repo.list_for_user(&current_user)?;
        Ok(channels)
    }

    /// The channels the sidebar lists, excluding any that belong to a custom
    /// sidebar section: the ones we are in, and the ones we used to be in
    /// (left, removed, banned, deleted), which stay listed read-only until
    /// their chat is deleted, as Signal keeps an ended group.
    pub fn get_unsectioned_channels_for_user(&self) -> Result<Vec<Channel>, ChannelError> {
        let current_user = self.settings_repo.current_user_id()?;
        let mut channels = self.channel_repo.list_for_user(&current_user)?;
        channels.extend(self.channel_repo.list_former_for_user(&current_user)?);
        let sectioned = self
            .section_repo
            .all_conversation_ids()
            .map_err(ChannelError::Storage)?;
        Ok(channels
            .into_iter()
            .filter(|ch| !sectioned.contains(&ch.id.to_string()))
            .collect())
    }

    /// Get channels sorted by most recently used (last_message_at DESC).
    ///
    /// Channels with messages appear first (sorted by recency).
    /// Channels without messages appear at the end (sorted by creation time).
    pub fn get_channels_mru(&self) -> Result<Vec<Channel>, ChannelError> {
        let mut channels = self.get_unsectioned_channels_for_user()?;
        channels.sort_by(|a, b| match (&b.last_message_at, &a.last_message_at) {
            (Some(b_time), Some(a_time)) => b_time.cmp(a_time),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b.created_at.cmp(&a.created_at),
        });
        Ok(channels)
    }

    /// Get a channel from local storage (no network request).
    pub fn get_channel_local(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<Channel>, ChannelError> {
        let channel = self.channel_repo.get(channel_id)?;
        Ok(channel)
    }

    /// List channel members from local storage (no network request).
    pub fn list_members_local(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Vec<crate::domain::channel::Membership>, ChannelError> {
        let members = self.channel_repo.list_members(channel_id)?;
        Ok(members)
    }

    /// Get the set of member user IDs for a channel (local query).
    ///
    /// Used for filtering invite candidates — callers can check which users
    /// are already members without loading full membership objects.
    pub fn get_member_ids(
        &self,
        channel_id: &ChannelId,
    ) -> Result<std::collections::HashSet<crate::domain::ids::UserId>, ChannelError> {
        let members = self.channel_repo.list_members(channel_id)?;
        Ok(members.into_iter().map(|m| m.user_id).collect())
    }

    /// Get the peer's user ID from a DM channel (local query).
    ///
    /// `None` unless the channel is a `DirectMessage`: a public or private
    /// channel is never a 1:1, however few members it has. Answering by
    /// membership alone routed every channel call through the DM path, so it
    /// rang one member and never posted the channel's call rows (O9).
    pub fn get_dm_peer_local(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<crate::domain::ids::UserId>, ChannelError> {
        let is_dm = self
            .channel_repo
            .get(channel_id)?
            .is_some_and(|c| c.channel_type == ChannelType::DirectMessage);
        if !is_dm {
            return Ok(None);
        }
        let current_user_id = self.settings_repo.current_user_id()?;
        let memberships = self.channel_repo.list_members(channel_id)?;

        for membership in memberships {
            if membership.user_id != current_user_id {
                return Ok(Some(membership.user_id));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::domain::ids::{ChannelId, UserId};
    use crate::storage::repositories::settings_repo::keys;

    /// A two-member channel of `channel_type` holding us and `peer`.
    fn client_with(
        channel_type: ChannelType,
    ) -> (
        std::sync::Arc<crate::client::ChatterClient>,
        tempfile::TempDir,
        ChannelId,
        UserId,
    ) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let (me, peer, channel) = (UserId::new(), UserId::new(), ChannelId::new());
        let svc = client.channels();
        svc.settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        svc.channel_repo
            .insert(&Channel::new(channel, "c".into(), channel_type, me))
            .unwrap();
        for user_id in [me, peer] {
            svc.channel_repo
                .insert_membership(&Membership {
                    channel_id: channel,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
        }
        (client, dir, channel, peer)
    }

    /// A two-member public or private channel is a channel, not a DM: its
    /// call must take the group path (which posts the O9 call rows).
    #[tokio::test]
    async fn a_two_member_channel_has_no_dm_peer() {
        for channel_type in [ChannelType::Public, ChannelType::Private] {
            let (client, _dir, channel, _) = client_with(channel_type);
            assert_eq!(
                client.channels().get_dm_peer_local(&channel).unwrap(),
                None,
                "{channel_type:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_dm_channel_names_the_other_member() {
        let (client, _dir, channel, peer) = client_with(ChannelType::DirectMessage);
        assert_eq!(
            client.channels().get_dm_peer_local(&channel).unwrap(),
            Some(peer)
        );
    }
}
