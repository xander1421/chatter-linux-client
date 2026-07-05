//! Channel gRPC client.
//!
//! Client for channel service operations including channel management,
//! membership, moderation, and event streaming.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    channel, channel_service_client::ChannelServiceClient, ArchiveChannelRequest,
    ArchiveChannelResponse, BanUserRequest, Channel as ChannelProto, ChannelEvent,
    ChannelEventRequest, ChannelJoinRequest, ChannelSearchRequest, ChannelSearchResponse,
    ChannelType, CreateChannelRequest, DeleteChannelRequest, GetChannelMembersRequest,
    GetChannelMembersResponse, GetChannelRequest, InviteResponse, InviteUserRequest,
    KickUserRequest, LeaveChannelRequest, ListArchivedChannelsRequest,
    ListArchivedChannelsResponse, ListChannelsRequest, ListChannelsResponse, UnarchiveChannelRequest,
    UnarchiveChannelResponse, UnbanUserRequest, UpdateChannelRequest, UpdateMemberRoleRequest,
    ChannelRole,
};

/// Channel service client wrapper.
///
/// Wraps the generated ChannelServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut channel = ChannelClient::new(pool.channel(), pool.auth_clone());
/// let channel_events = channel.create_channel("my-channel", ChannelType::ChannelPublic, None, vec![], None, None, None).await?;
/// ```
pub struct ChannelClient {
    inner: ChannelServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl ChannelClient {
    /// Create a new ChannelClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: ChannelServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Channel Management RPCs ====================

    /// Create a new channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with creation progress and final channel.
    ///
    /// # Arguments
    ///
    /// * `name` - Channel name
    /// * `channel_type` - Channel type (Public, Private, DirectMessage)
    /// * `config` - Optional channel configuration
    /// * `initial_member_ids` - Optional list of initial member IDs
    /// * `password` - Optional password for private channels
    /// * `description` - Optional channel description
    /// * `icon` - Optional raw image bytes for the channel icon (≤64KB)
    #[allow(clippy::too_many_arguments)]
    pub async fn create_channel(
        &mut self,
        name: &str,
        channel_type: ChannelType,
        config: Option<channel::Config>,
        initial_member_ids: Vec<String>,
        password: Option<&str>,
        description: Option<&str>,
        icon: Option<&[u8]>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>> + use<>, NetworkError> {
        let request = CreateChannelRequest {
            name: name.to_string(),
            r#type: channel_type.into(),
            config,
            initial_member_ids,
            password: password.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            description: description.unwrap_or("").to_string(),
            icon: icon.map(<[u8]>::to_vec).unwrap_or_default(),
        };

        let response = self
            .inner
            .create_channel(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Get a channel by ID.
    pub async fn get_channel(&mut self, channel_id: &str) -> Result<ChannelProto, NetworkError> {
        let request = GetChannelRequest {
            channel_id: channel_id.to_string(),
        };

        self.inner
            .get_channel(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Update a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with update progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel ID to update
    /// * `channel` - Updated channel data
    /// * `update_mask` - Optional field mask specifying which fields to update
    pub async fn update_channel(
        &mut self,
        channel_id: &str,
        channel: ChannelProto,
        update_mask: Option<prost_types::FieldMask>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = UpdateChannelRequest {
            channel_id: channel_id.to_string(),
            channel: Some(channel),
            update_mask,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .update_channel(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Delete a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with deletion progress.
    pub async fn delete_channel(
        &mut self,
        channel_id: &str,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = DeleteChannelRequest {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .delete_channel(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// List channels with pagination.
    ///
    /// # Arguments
    ///
    /// * `limit` - Max channels to return
    /// * `offset` - Pagination offset
    /// * `archived` - Optional filter: Some(true)=archived only, Some(false)=non-archived only, None=all
    pub async fn list_channels(
        &mut self,
        limit: i32,
        offset: i32,
        archived: Option<bool>,
    ) -> Result<ListChannelsResponse, NetworkError> {
        let request = ListChannelsRequest {
            limit,
            offset,
            archived,
        };

        self.inner
            .list_channels(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Search for channels by query.
    ///
    /// # Arguments
    ///
    /// * `query` - Search query
    /// * `limit` - Max results to return
    /// * `offset` - Pagination offset
    /// * `persistence_filter` - Optional persistence type filters
    /// * `type_filter` - Optional channel type filters
    pub async fn search_channels(
        &mut self,
        query: &str,
        limit: i32,
        offset: i32,
        persistence_filter: Vec<channel::config::Persistence>,
        type_filter: Vec<ChannelType>,
    ) -> Result<ChannelSearchResponse, NetworkError> {
        let request = ChannelSearchRequest {
            query: query.to_string(),
            limit,
            offset,
            persistence_filter: persistence_filter.into_iter().map(|p| p.into()).collect(),
            type_filter: type_filter.into_iter().map(|t| t.into()).collect(),
        };

        self.inner
            .search_channels(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Membership RPCs ====================

    /// Join a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with join progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to join
    /// * `password` - Optional password for password-protected channels
    /// * `start_streaming` - Whether to start streaming channel events after joining
    pub async fn join_channel(
        &mut self,
        channel_id: &str,
        password: Option<&str>,
        start_streaming: bool,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = ChannelJoinRequest {
            channel_id: channel_id.to_string(),
            password: password.unwrap_or("").to_string(),
            start_streaming,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .join_channel(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Leave a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with leave progress.
    pub async fn leave_channel(
        &mut self,
        channel_id: &str,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = LeaveChannelRequest {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .leave_channel(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Invite a user to a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with invitation progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to invite to
    /// * `invitee_id` - User ID to invite
    /// * `message` - Optional invitation message
    pub async fn invite_user(
        &mut self,
        channel_id: &str,
        invitee_id: &str,
        message: Option<&str>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = InviteUserRequest {
            channel_id: channel_id.to_string(),
            invitee_id: invitee_id.to_string(),
            message: message.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .invite_user(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Respond to a channel invitation (streaming response).
    ///
    /// Returns stream of ChannelEvent with response progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel being invited to
    /// * `accept` - True to accept, false to decline
    pub async fn respond_to_invite(
        &mut self,
        channel_id: &str,
        accept: bool,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = InviteResponse {
            channel_id: channel_id.to_string(),
            accept,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .respond_to_invite(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// List the current user's pending channel invitations (unary).
    /// The server scopes to `status = PENDING AND expires_at > NOW()` for the
    /// authenticated user.
    pub async fn list_pending_invites(
        &mut self,
    ) -> Result<crate::proto::chat_v1::ListPendingInvitesResponse, NetworkError> {
        let response = self
            .inner
            .list_pending_invites(crate::proto::chat_v1::ListPendingInvitesRequest {})
            .await
            .map_err(|s| NetworkError::from_status(&s))?;
        Ok(response.into_inner())
    }

    /// Kick a user from a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with kick progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to kick from
    /// * `user_id` - User to kick
    /// * `reason` - Optional kick reason
    pub async fn kick_user(
        &mut self,
        channel_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = KickUserRequest {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            reason: reason.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .kick_user(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Ban a user from a channel.
    ///
    /// Note: Unlike other membership RPCs, this returns () not a stream.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to ban from
    /// * `user_id` - User to ban
    /// * `duration` - Optional ban duration (None = permanent)
    /// * `reason` - Optional ban reason
    pub async fn ban_user(
        &mut self,
        channel_id: &str,
        user_id: &str,
        duration: Option<std::time::Duration>,
        reason: Option<&str>,
    ) -> Result<(), NetworkError> {
        let request = BanUserRequest {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            duration: duration.map(|d| prost_types::Duration {
                seconds: d.as_secs() as i64,
                nanos: d.subsec_nanos() as i32,
            }),
            reason: reason.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .ban_user(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Unban a user from a channel (streaming response).
    ///
    /// Returns stream of ChannelEvent with unban progress.
    pub async fn unban_user(
        &mut self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = UnbanUserRequest {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .unban_user(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Get channel members with pagination.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to get members from
    /// * `limit` - Max members to return
    /// * `offset` - Pagination offset
    pub async fn get_channel_members(
        &mut self,
        channel_id: &str,
        limit: i32,
        offset: i32,
    ) -> Result<GetChannelMembersResponse, NetworkError> {
        let request = GetChannelMembersRequest {
            channel_id: channel_id.to_string(),
            limit,
            offset,
        };

        self.inner
            .get_channel_members(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Update a member's role (streaming response).
    ///
    /// Returns stream of ChannelEvent with role update progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel containing the member
    /// * `user_id` - User whose role to update
    /// * `new_role` - The new role to assign
    pub async fn update_member_role(
        &mut self,
        channel_id: &str,
        user_id: &str,
        new_role: ChannelRole,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = UpdateMemberRoleRequest {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            new_role: new_role.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .update_member_role(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    // ==================== Event Streaming ====================

    /// Stream channel events.
    ///
    /// Long-lived stream for real-time channel updates.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Optional channel ID. If not provided, streams events for all channels user has access to.
    pub async fn stream_channel_events(
        &mut self,
        channel_id: Option<&str>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = ChannelEventRequest {
            channel_id: channel_id.unwrap_or("").to_string(),
        };

        let response = self
            .inner
            .stream_channel_events(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    // ==================== Archive RPCs ====================

    /// Archive a channel.
    pub async fn archive_channel(
        &mut self,
        channel_id: &str,
    ) -> Result<ArchiveChannelResponse, NetworkError> {
        let request = ArchiveChannelRequest {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .archive_channel(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Unarchive a channel.
    pub async fn unarchive_channel(
        &mut self,
        channel_id: &str,
    ) -> Result<UnarchiveChannelResponse, NetworkError> {
        let request = UnarchiveChannelRequest {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .unarchive_channel(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List archived channels with pagination.
    ///
    /// # Arguments
    ///
    /// * `limit` - Max channels to return
    /// * `offset` - Pagination offset
    pub async fn list_archived_channels(
        &mut self,
        limit: i32,
        offset: i32,
    ) -> Result<ListArchivedChannelsResponse, NetworkError> {
        let request = ListArchivedChannelsRequest { limit, offset };

        self.inner
            .list_archived_channels(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_channel_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = ChannelClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_channel_type_values() {
        // Verify ChannelType enum is accessible and has expected values.
        // DIRECT_MESSAGE (3) was removed from the wire enum — DMs are
        // channel-free (Signal-style).
        assert_eq!(ChannelType::Unspecified as i32, 0);
        assert_eq!(ChannelType::ChannelPublic as i32, 1);
        assert_eq!(ChannelType::ChannelPrivate as i32, 2);
        assert_eq!(ChannelType::SavedMessages as i32, 4);
    }

    #[test]
    fn test_channel_role_values() {
        // Verify ChannelRole enum is accessible and has expected values
        assert_eq!(ChannelRole::RoleUnspecified as i32, 0);
        assert_eq!(ChannelRole::Member as i32, 1);
        assert_eq!(ChannelRole::Moderator as i32, 2);
        assert_eq!(ChannelRole::Admin as i32, 3);
        assert_eq!(ChannelRole::Owner as i32, 4);
    }

    #[test]
    fn test_channel_construction() {
        // Verify Channel struct can be constructed
        use crate::proto::chat_v1::Channel as ChannelProto;

        let channel = ChannelProto {
            id: "channel-123".to_string(),
            r#type: ChannelType::ChannelPublic as i32,
            name: "test-channel".to_string(),
            config: None,
            owner_id: "user-456".to_string(),
            members: vec![],
            created_at: 0,
            description: "A test channel".to_string(),
            icon: Vec::new(),
            archived: false,
        };

        assert_eq!(channel.id, "channel-123");
        assert_eq!(channel.name, "test-channel");
        assert_eq!(channel.r#type, ChannelType::ChannelPublic as i32);
    }

    #[test]
    fn test_channel_config_construction() {
        // Verify Channel.Config struct can be constructed
        use crate::proto::chat_v1::channel::{config::Persistence, Config};

        let config = Config {
            persistence: Persistence::Persistent as i32,
            ttl: None,
            max_members: 100,
            encrypted: true,
            password_hash: "".to_string(),
            friends_only: false,
        };

        assert_eq!(config.persistence, Persistence::Persistent as i32);
        assert_eq!(config.max_members, 100);
        assert!(config.encrypted);
    }

    #[test]
    fn test_channel_persistence_values() {
        // Verify Persistence enum values
        use crate::proto::chat_v1::channel::config::Persistence;

        assert_eq!(Persistence::Unspecified as i32, 0);
        assert_eq!(Persistence::Ephemeral as i32, 1);
        assert_eq!(Persistence::Persistent as i32, 2);
    }

    #[test]
    fn test_channel_member_construction() {
        // Verify ChannelMember struct can be constructed
        use crate::proto::chat_v1::channel::{channel_member::InvitationStatus, ChannelMember};

        let member = ChannelMember {
            user_id: "user-123".to_string(),
            role: ChannelRole::Member as i32,
            joined_at: 1234567890,
            muted: false,
            banned: false,
            ban_expires_at: 0,
            invitation_status: InvitationStatus::Active as i32,
        };

        assert_eq!(member.user_id, "user-123");
        assert_eq!(member.role, ChannelRole::Member as i32);
        assert_eq!(member.invitation_status, InvitationStatus::Active as i32);
    }

    #[test]
    fn test_invitation_status_values() {
        // Verify InvitationStatus enum values
        use crate::proto::chat_v1::channel::channel_member::InvitationStatus;

        assert_eq!(InvitationStatus::Unspecified as i32, 0);
        assert_eq!(InvitationStatus::Active as i32, 1);
        assert_eq!(InvitationStatus::Pending as i32, 2);
        assert_eq!(InvitationStatus::Declined as i32, 3);
    }
}
