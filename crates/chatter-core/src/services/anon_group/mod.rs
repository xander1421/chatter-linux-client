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

/// A facade operation's account and exact authenticated session, captured before
/// waiting for either invitation serialization or lifecycle admission.
pub(crate) struct InviteSession<'a> {
    pub auth: &'a crate::state::AuthState,
    pub lifecycle: &'a tokio::sync::Mutex<()>,
    pub user: crate::domain::UserId,
    pub revision: u64,
}

impl InviteSession<'_> {
    pub(crate) async fn admit(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, AnonGroupError> {
        let guard = self.lifecycle.lock().await;
        let now = self.auth.get();
        if now.current_user_id != Some(self.user) || now.session_revision != self.revision {
            return Err(AnonGroupError::NotLoggedIn);
        }
        Ok(guard)
    }
}

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
    invite_acceptance: tokio::sync::Mutex<()>,
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

    pub async fn decline_pending_invite(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Result<(), AnonGroupError> {
        self.decline_pending_invite_bound(group_id, None).await
    }

    pub(crate) async fn decline_pending_invite_bound(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        session: Option<&InviteSession<'_>>,
    ) -> Result<(), AnonGroupError> {
        let admission = match session {
            Some(session) => Some(session.admit().await?),
            None => None,
        };
        let invite = self
            .anon_group_repo
            .get_pending_invite(group_id)?
            .ok_or_else(|| AnonGroupError::InvalidData("Invitation no longer pending".into()))?;
        if invite.invite_epoch == 0 {
            return Err(AnonGroupError::InvalidData(
                "Invitation has no admission epoch".into(),
            ));
        }
        self.anon_group_repo.decline_exact_pending_invite(&invite)?;
        drop(admission);
        let content = crate::proto::content_v1::Content {
            content: Some(
                crate::proto::content_v1::content::Content::AnonGroupDecline(
                    crate::proto::content_v1::AnonGroupDecline {
                        group_id: group_id.to_vec(),
                        invite_epoch: invite.invite_epoch,
                    },
                ),
            ),
            ephemeral_reply_to: None,
            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };
        self.send_to_anon_mailbox(
            &invite.inviter_id,
            Some(invite.inviter_device_id),
            &invite.inviter_mailbox_pk,
            content,
        )
        .await?;
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
        self.accept_pending_invite_bound(invite, None).await
    }

    pub(crate) async fn accept_pending_invite_bound(
        &self,
        invite: PendingAnonGroupInvite,
        session: Option<&InviteSession<'_>>,
    ) -> Result<crate::domain::anon_group::AnonGroup, AnonGroupError> {
        let group = self
            .accept_invite_bound(
                invite.group_id,
                invite.group_name.clone(),
                invite.inviter_id,
                invite.inviter_id,
                invite.inviter_device_id,
                invite.inviter_mailbox_pk,
                invite.profile_key,
                invite.inviter_display_name.clone(),
                invite.inviter_avatar.clone(),
                Some(&invite),
                session,
            )
            .await
            .inspect_err(AnonGroupError::trace_invite_failure)?;
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
            invite_acceptance: tokio::sync::Mutex::new(()),
        }
    }

    /// Generate random padding to reach a target size.
    ///
    /// Delegates to the centralized NDSS '21 padding in `gateway::builders`.
    pub(crate) fn generate_padding(boundary: usize) -> Vec<u8> {
        crate::services::gateway::builders::generate_padding(boundary)
    }
}
