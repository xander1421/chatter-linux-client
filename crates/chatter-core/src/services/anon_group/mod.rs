//! Anonymous group service.
//!
//! NDSS'21 style groups where the server has no knowledge of group existence.
//! All group operations are performed via pairwise sealed sender messages.
//!
//! # Architecture
//!
//! This service uses `PairwiseService` for Signal Protocol encryption, keeping
//! a clean separation from DM-specific infrastructure:
//!
//! - **Invites**: Over Tor via the invitee's anon DM tunnel, else anonymous sealed
//!   sender to their device inbox (they have no group mailbox yet); never the friend DM
//! - **Group messages**: Encrypted with Signal Protocol, delivered to member's anonymous mailbox
//!
//! The anonymous mailbox per group ensures the server cannot correlate group membership.

mod control;
mod ephemeral;
mod error;
mod handlers;
mod held;
mod lifecycle;
mod membership;
mod messaging;
mod processing;
mod removal;
pub(crate) mod send_ledger;
mod storage;

pub use error::AnonGroupError;

use std::sync::Arc;

use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pairwise::PairwiseService;
use crate::storage::repositories::AnonDmRepository;
use crate::storage::repositories::AnonGroupRepository;
use crate::storage::repositories::AttachmentRepository;
use crate::storage::repositories::SettingsRepository;
use crate::storage::repositories::anon_group_repo::PendingAnonGroupInvite;

/// Anonymous group service.
///
/// Handles all anonymous group operations following the NDSS'21 model.
///
/// # Separation of Concerns
///
/// - Uses `PairwiseService` for Signal Protocol encryption (reusable across DM/groups)
/// - Uses `MailboxManager` for delivery to anonymous mailboxes
/// - Does NOT go through DM infrastructure for group messages
pub struct AnonGroupService {
    pub(crate) settings_repo: Arc<SettingsRepository>,
    pub(crate) anon_group_repo: Arc<AnonGroupRepository>,
    /// For routing invites through an existing anon DM tunnel.
    pub(crate) anon_dm_repo: Arc<AnonDmRepository>,
    /// Attachment storage for incoming file metadata.
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    /// Message storage shared with the rest of the client.
    pub(crate) message_service: Arc<MessageService>,
    /// Reusable Signal Protocol pairwise encryption.
    pub(crate) pairwise_service: Arc<PairwiseService>,
    /// For sending to anonymous mailboxes.
    pub(crate) mailbox_manager: Arc<MailboxManager>,
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) token_manager: Arc<TokenManager>,
    /// Posts from senders outside the roster, awaiting a member list.
    pub(crate) held_posts: held::HeldPosts,
}

impl AnonGroupService {
    pub fn save_pending_invite(
        &self,
        invite: &PendingAnonGroupInvite,
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo.save_pending_invite(invite)?;
        Ok(())
    }

    pub fn list_pending_invites(&self) -> Result<Vec<PendingAnonGroupInvite>, AnonGroupError> {
        Ok(self.anon_group_repo.list_pending_invites()?)
    }

    pub fn decline_pending_invite(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo.remove_pending_invite(group_id)?;
        Ok(())
    }

    pub fn update_pending_invite_avatar(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        avatar: &[u8],
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo
            .update_pending_invite_avatar(group_id, avatar)?;
        Ok(())
    }

    pub async fn accept_pending_invite(
        &self,
        group_id: crate::domain::anon_group::AnonGroupId,
    ) -> Result<crate::domain::anon_group::AnonGroup, AnonGroupError> {
        let invite = self
            .anon_group_repo
            .get_pending_invite(&group_id)?
            .ok_or_else(|| AnonGroupError::InvalidData("Invitation no longer pending".into()))?;
        let group = self
            .accept_invite(
                invite.group_id,
                invite.group_name,
                invite.inviter_id,
                invite.inviter_id,
                invite.inviter_device_id,
                invite.inviter_mailbox_pk,
                invite.profile_key,
                invite.inviter_display_name,
                invite.inviter_avatar,
            )
            .await?;
        self.decline_pending_invite(&group_id)?;
        Ok(group)
    }
    /// Create a new AnonGroupService.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        anon_group_repo: Arc<AnonGroupRepository>,
        anon_dm_repo: Arc<AnonDmRepository>,
        attachment_repo: Arc<AttachmentRepository>,
        message_service: Arc<MessageService>,
        pairwise_service: Arc<PairwiseService>,
        mailbox_manager: Arc<MailboxManager>,
        grpc_pool: Arc<GrpcClientPool>,
        token_manager: Arc<TokenManager>,
    ) -> Self {
        Self {
            settings_repo,
            anon_group_repo,
            anon_dm_repo,
            attachment_repo,
            message_service,
            pairwise_service,
            mailbox_manager,
            grpc_pool,
            token_manager,
            held_posts: held::HeldPosts::default(),
        }
    }

    /// Generate random padding to reach a target size.
    ///
    /// Delegates to the centralized NDSS '21 padding in `gateway::builders`.
    pub(crate) fn generate_padding(boundary: usize) -> Vec<u8> {
        crate::services::gateway::builders::generate_padding(boundary)
    }
}
