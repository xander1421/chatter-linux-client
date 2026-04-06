//! Channel listing and archive operations.
//!
//! List, search, and archive methods for ChannelService.

use crate::domain::channel::{Channel, ChannelType};
use crate::domain::ids::ChannelId;
use crate::proto::chat_v1;
use crate::services::current_user::CurrentUserProvider;

use crate::services::channel::converters::{domain_channel_type_to_proto, proto_to_domain_channel, proto_to_domain_membership};
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
            .search_channels(query, limit as i32, offset as i32, vec![], proto_type_filter)
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

    /// Get channels for the current user, excluding any that belong to a
    /// custom sidebar section.
    pub fn get_unsectioned_channels_for_user(&self) -> Result<Vec<Channel>, ChannelError> {
        let channels = self.get_channels_for_user()?;
        let sectioned = self.section_repo.all_conversation_ids()
            .unwrap_or_default();
        Ok(channels.into_iter().filter(|ch| !sectioned.contains(&ch.id.to_string())).collect())
    }

    /// Get channels sorted by most recently used (last_message_at DESC).
    ///
    /// Channels with messages appear first (sorted by recency).
    /// Channels without messages appear at the end (sorted by creation time).
    pub fn get_channels_mru(&self) -> Result<Vec<Channel>, ChannelError> {
        let mut channels = self.get_unsectioned_channels_for_user()?;
        channels.sort_by(|a, b| {
            match (&b.last_message_at, &a.last_message_at) {
                (Some(b_time), Some(a_time)) => b_time.cmp(a_time),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => b.created_at.cmp(&a.created_at),
            }
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

    /// List channel members with profile information (display name, avatar).
    ///
    /// Joins membership data with user profiles from local storage.
    /// Members without a stored profile get a fallback display name from their user ID.
    pub fn list_members_with_profiles(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Vec<crate::domain::channel::MemberWithProfile>, ChannelError> {
        let members = self.channel_repo.list_members(channel_id)?;
        let mut result = Vec::with_capacity(members.len());
        for membership in members {
            let profile = self.user_repo.get_profile(&membership.user_id).ok().flatten();
            let display_name = profile.as_ref()
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| format!("User {}", &membership.user_id.to_string()[..8]));
            let avatar_url = profile.and_then(|p| p.avatar_url);
            result.push(crate::domain::channel::MemberWithProfile {
                membership,
                display_name,
                avatar_url,
            });
        }
        Ok(result)
    }

    /// Get the set of member user IDs for a channel (local query).
    ///
    /// Used for filtering invite candidates — callers can check which users
    /// are already members without loading full membership objects.
    pub fn get_member_ids(&self, channel_id: &ChannelId) -> Result<std::collections::HashSet<crate::domain::ids::UserId>, ChannelError> {
        let members = self.channel_repo.list_members(channel_id)?;
        Ok(members.into_iter().map(|m| m.user_id).collect())
    }

    /// Get the peer's user ID from a DM channel (local query).
    ///
    /// DM channels have exactly 2 members - returns the one that's not the current user.
    pub fn get_dm_peer_local(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<crate::domain::ids::UserId>, ChannelError> {
        let current_user_id = self.settings_repo.current_user_id()?;
        let memberships = self.channel_repo.list_members(channel_id)?;

        for membership in memberships {
            if membership.user_id != current_user_id {
                return Ok(Some(membership.user_id));
            }
        }

        Ok(None)
    }

    /// Find the DM channel with a specific peer (local query).
    ///
    /// Looks up the DM channel where both the current user and the peer are members.
    pub fn get_dm_channel_for_peer(
        &self,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<Option<crate::domain::channel::Channel>, ChannelError> {
        let current_user_id = self.settings_repo.current_user_id()?;
        self.channel_repo
            .get_dm_channel_by_peer(&current_user_id, peer_id)
            .map_err(ChannelError::from)
    }
}
