//! Channel service module.
//!
//! Provides channel creation, management, membership, and moderation operations.

pub mod converters;
pub mod error;
pub mod service;

// Re-export main types for convenience
pub use converters::proto_to_domain_channel;
pub use error::ChannelError;
pub use service::ChannelService;

/// Record a completed ownership transfer locally: the channel's `owner_id`
/// (which every owner-only permission reads) and the two members' roles. Run
/// by the transferring client after its RPC succeeds and by every member on
/// the server's `OwnershipTransferred`; idempotent. A membership not stored
/// here is skipped (the next roster sync brings it with its role).
pub(crate) fn apply_ownership_transfer(
    repo: &crate::storage::repositories::channel_repo::ChannelRepository,
    channel_id: &crate::domain::ids::ChannelId,
    from: &crate::domain::ids::UserId,
    to: &crate::domain::ids::UserId,
) -> Result<(), crate::error::StorageError> {
    use crate::domain::channel::MemberRole;
    if let Some(mut channel) = repo.get(channel_id)? {
        if channel.owner_id != *to {
            channel.owner_id = *to;
            repo.update(&channel)?;
        }
    }
    for (user, role) in [(from, MemberRole::Admin), (to, MemberRole::Owner)] {
        if let Some(mut membership) = repo.get_membership(channel_id, user)? {
            if membership.role != role {
                membership.role = role;
                repo.update_membership(&membership)?;
            }
        }
    }
    Ok(())
}
