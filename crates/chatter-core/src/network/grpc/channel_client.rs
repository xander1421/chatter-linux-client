//! Channel gRPC client.
//!
//! Client for channel service operations including channel management,
//! membership, moderation, and event streaming.

use crate::network::grpc::transport::GrpcTransport;
use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    ArchiveChannelRequest, ArchiveChannelResponse, BanUserRequest, BannedUser,
    Channel as ChannelProto, ChannelChange, ChannelEvent, ChannelEventRequest, ChannelJoinRequest,
    ChannelSearchRequest, ChannelSearchResponse, ChannelType, CreateChannelRequest,
    DeleteChannelRequest, GetChannelChangesRequest, GetChannelChangesResponse,
    GetChannelMembersRequest, GetChannelMembersResponse, GetChannelRequest, GetChannelStateRequest,
    InviteResponse, InviteUserRequest, KickUserRequest, LeaveChannelRequest,
    ListArchivedChannelsRequest, ListArchivedChannelsResponse, ListBannedUsersRequest,
    ListChannelsRequest, ListChannelsResponse, MembershipState, MuteUserRequest,
    SubmitChannelChangeRequest, TransferOwnershipRequest, UnarchiveChannelRequest,
    UnarchiveChannelResponse, UnbanUserRequest, UnmuteUserRequest, UpdateChannelRequest,
    UpdateMemberRoleRequest, channel, channel_service_client::ChannelServiceClient,
};

/// Whether a membership RPC was refused because its change is not chained to
/// the server's current head (`ABORTED` + `ChannelChangeConflict`): fetch the
/// missing changes, re-validate, re-sign and retry.
pub fn is_change_conflict(e: &NetworkError) -> bool {
    matches!(e, NetworkError::ServerError(m) if m.starts_with("aborted:"))
}

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
/// let channel_events = channel.create_channel("my-channel", ChannelType::ChannelPublic, None, None, None, genesis).await?;
/// ```
pub struct ChannelClient {
    inner: ChannelServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl ChannelClient {
    /// Create a new ChannelClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
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
    /// * `channel_type` - Channel type (Public, Private)
    /// * `config` - Optional channel configuration
    /// * `description` - Optional channel description
    /// * `icon` - Optional raw image bytes for the channel icon (≤64KB)
    /// * `genesis` - Revision 1 of the membership log (public/private
    ///   channels; it names the channel id). Initial members are invited
    ///   afterwards with signed `InviteUser` changes.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_channel(
        &mut self,
        name: &str,
        channel_type: ChannelType,
        config: Option<channel::Config>,
        description: Option<&str>,
        icon: Option<&[u8]>,
        genesis: Option<ChannelChange>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>> + use<>, NetworkError> {
        let request = CreateChannelRequest {
            name: name.to_string(),
            r#type: channel_type.into(),
            config,
            genesis,
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
    /// * `change` - The joiner's signed `JoinViaInvite` (or `AcceptInvite`)
    /// * `start_streaming` - Whether to start streaming channel events after joining
    pub async fn join_channel(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
        start_streaming: bool,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = ChannelJoinRequest {
            channel_id: channel_id.to_string(),
            start_streaming,
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
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
        change: ChannelChange,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = LeaveChannelRequest {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
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
    /// * `change` - The inviter's signed `AddMember{pending: true}` (names the invitee)
    /// * `message` - Optional invitation message
    pub async fn invite_user(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
        message: Option<&str>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = InviteUserRequest {
            channel_id: channel_id.to_string(),
            message: message.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
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
    /// * `change` - Our signed `AcceptInvite`, or `RemoveMember{self, LEFT}` to decline
    pub async fn respond_to_invite(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = InviteResponse {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
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
    /// * `change` - The moderator's signed `RemoveMember{KICKED}` (names the target)
    /// * `reason` - Optional kick reason
    pub async fn kick_user(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
        reason: Option<&str>,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = KickUserRequest {
            channel_id: channel_id.to_string(),
            reason: reason.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
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
    /// * `change` - The moderator's signed `RemoveMember{BANNED}` (names the target)
    /// * `reason` - Optional ban reason
    ///
    /// Always permanent (until unbanned): no caller offers a timed ban.
    pub async fn ban_user(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
        reason: Option<&str>,
    ) -> Result<(), NetworkError> {
        let request = BanUserRequest {
            channel_id: channel_id.to_string(),
            duration: None,
            reason: reason.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
        };

        self.inner
            .ban_user(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Hand the channel to the member `change` names; the caller (the owner)
    /// becomes an admin. Everyone learns it from the relayed `ChannelChange`.
    pub async fn transfer_ownership(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
    ) -> Result<(), NetworkError> {
        let request = TransferOwnershipRequest {
            channel_id: channel_id.to_string(),
            change: Some(change),
        };
        self.inner
            .transfer_ownership(request)
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

    /// Mute a channel member for `duration`, or until unmuted when `None`.
    /// Server-enforced: SpiceDB `send_message` excludes `muted`, and the
    /// server's expiry sweep lifts a timed mute once it lapses.
    pub async fn mute_user(
        &mut self,
        channel_id: &str,
        user_id: &str,
        duration: Option<std::time::Duration>,
    ) -> Result<(), NetworkError> {
        let request = MuteUserRequest {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            duration: duration.map(|d| prost_types::Duration {
                seconds: d.as_secs() as i64,
                nanos: d.subsec_nanos() as i32,
            }),
        };
        self.inner
            .mute_user(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Lift a mute.
    pub async fn unmute_user(
        &mut self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<(), NetworkError> {
        let request = UnmuteUserRequest {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
        };
        self.inner
            .unmute_user(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// A channel's active bans (owner/admin only).
    pub async fn list_banned_users(
        &mut self,
        channel_id: &str,
    ) -> Result<Vec<BannedUser>, NetworkError> {
        let request = ListBannedUsersRequest {
            channel_id: channel_id.to_string(),
        };
        self.inner
            .list_banned_users(request)
            .await
            .map(|r| r.into_inner().bans)
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Why WE are no longer in the channel: the signed change that removed
    /// us (with the state it applied to), or the channel's deletion.
    pub async fn get_departure(
        &mut self,
        channel_id: &str,
    ) -> Result<crate::proto::chat_v1::Departure, NetworkError> {
        let request = crate::proto::chat_v1::GetDepartureRequest {
            channel_id: channel_id.to_string(),
        };
        self.inner
            .get_departure(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
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

    /// Get EVERY channel member, paginating until the server is exhausted.
    ///
    /// Use this wherever the answer has to be "everyone" — SKDM recipient
    /// sync, group-call key distribution and rotation, channel content
    /// fan-out. A single `get_channel_members(_, 100, 0)` call silently
    /// truncates at the page size, and in those callers a member past the
    /// first page becomes a member who cannot decrypt (OPEN-ITEMS §16). The
    /// paged variant above is for UI that genuinely wants one page.
    pub async fn get_all_channel_members(
        &mut self,
        channel_id: &str,
    ) -> Result<Vec<crate::proto::chat_v1::channel::ChannelMember>, NetworkError> {
        const PAGE_SIZE: i32 = 100;
        let mut members = Vec::new();
        let mut offset: i32 = 0;
        loop {
            let response = self
                .get_channel_members(channel_id, PAGE_SIZE, offset)
                .await?;
            let page_len = response.members.len();
            members.extend(response.members);
            if !fetch_next_members_page(page_len, PAGE_SIZE, members.len(), response.total) {
                break;
            }
            offset += PAGE_SIZE;
        }
        Ok(members)
    }

    /// Update a member's role (streaming response).
    ///
    /// Returns stream of ChannelEvent with role update progress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel containing the member
    /// * `change` - The admin's signed `SetRole` (names target and role)
    pub async fn update_member_role(
        &mut self,
        channel_id: &str,
        change: ChannelChange,
    ) -> Result<impl Stream<Item = Result<ChannelEvent, NetworkError>>, NetworkError> {
        let request = UpdateMemberRoleRequest {
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            change: Some(change),
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

    // ==================== Membership log ====================

    /// Append a log-only change (invites, policy, timer) or any single-action
    /// change. Returns the accepted revision.
    pub async fn submit_channel_change(
        &mut self,
        change: ChannelChange,
    ) -> Result<u64, NetworkError> {
        self.inner
            .submit_channel_change(SubmitChannelChangeRequest {
                change: Some(change),
            })
            .await
            .map(|r| r.into_inner().revision)
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Up to `limit` changes from `from_revision` (inclusive), ascending.
    /// `invite_pub` authorises a join-link holder who is not yet a member.
    pub async fn get_channel_changes(
        &mut self,
        channel_id: &str,
        from_revision: u64,
        limit: i32,
        invite_pub: &[u8],
    ) -> Result<GetChannelChangesResponse, NetworkError> {
        self.inner
            .get_channel_changes(GetChannelChangesRequest {
                channel_id: channel_id.to_string(),
                from_revision,
                limit,
                invite_pub: invite_pub.to_vec(),
            })
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// The server's membership state at `revision` (0 = current). Untrusted
    /// until a verified change chains to its hash.
    pub async fn get_channel_state(
        &mut self,
        channel_id: &str,
        revision: u64,
        invite_pub: &[u8],
    ) -> Result<MembershipState, NetworkError> {
        self.inner
            .get_channel_state(GetChannelStateRequest {
                channel_id: channel_id.to_string(),
                revision,
                invite_pub: invite_pub.to_vec(),
            })
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
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

/// Termination rule for [`ChannelClient::get_all_channel_members`], separated
/// so the arithmetic is testable without a server.
///
/// Stop on a short page (the server ran out), or once `total` — when the
/// server reports a positive one — has been fetched. The second condition is
/// what bounds the loop against a server that always fills pages regardless of
/// offset: without it, such a bug would loop forever; with it, the worst case
/// is one full sweep of the reported membership.
fn fetch_next_members_page(page_len: usize, page_size: i32, fetched: usize, total: i32) -> bool {
    if (page_len as i32) < page_size {
        return false;
    }
    if total > 0 && fetched >= total as usize {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::chat_v1::ChannelRole;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_channel_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
            let _client = ChannelClient::new(channel, interceptor);
        }
    }

    /// The exact miss OPEN-ITEMS §16 records: 100 members come back in one
    /// full page, and the old single-call pattern stopped there. A full first
    /// page with more reported must fetch again.
    #[test]
    fn a_full_first_page_with_more_members_keeps_paginating() {
        assert!(fetch_next_members_page(100, 100, 100, 250));
    }

    #[test]
    fn a_short_page_ends_the_sweep() {
        assert!(!fetch_next_members_page(37, 100, 137, 250));
        // Empty page too — a channel whose size is an exact multiple of the
        // page size ends on one of these rather than looping.
        assert!(!fetch_next_members_page(0, 100, 200, 200));
    }

    /// A membership that is an exact multiple of the page size, with a server
    /// that reports `total`: the sweep must stop at `total` without issuing
    /// the extra empty-page request being merely one wasted call — the case
    /// this guards is a server that ignores `offset` and returns a full page
    /// forever, where `total` is the only thing standing between one sweep
    /// and an infinite loop.
    #[test]
    fn reported_total_bounds_a_server_that_always_fills_pages() {
        assert!(!fetch_next_members_page(100, 100, 100, 100));
        assert!(!fetch_next_members_page(100, 100, 200, 150));
    }

    /// total == 0 or negative means "not reported" — only the short-page rule
    /// applies then, and a full page must keep going.
    #[test]
    fn an_unreported_total_falls_back_to_short_page_termination() {
        assert!(fetch_next_members_page(100, 100, 100, 0));
        assert!(fetch_next_members_page(100, 100, 300, -1));
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
        use crate::proto::chat_v1::channel::{Config, config::Persistence};

        let config = Config {
            persistence: Persistence::Persistent as i32,
            ttl: None,
            max_members: 100,
            encrypted: true,
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
        use crate::proto::chat_v1::channel::{ChannelMember, channel_member::InvitationStatus};

        let member = ChannelMember {
            user_id: "user-123".to_string(),
            role: ChannelRole::Member as i32,
            joined_at: 1234567890,
            muted: false,
            banned: false,
            ban_expires_at: 0,
            mute_expires_at: 0,
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
