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
//! - **Invites**: Sent via existing DM channel (recipient doesn't have group mailbox yet)
//! - **Group messages**: Encrypted with Signal Protocol, delivered to member's anonymous mailbox
//!
//! The anonymous mailbox per group ensures the server cannot correlate group membership.

mod control;
mod ephemeral;
mod error;
mod handlers;
mod lifecycle;
mod membership;
mod messaging;
mod removal;
mod storage;

pub use error::AnonGroupError;

use std::sync::Arc;

use rand::RngCore;

use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pairwise::PairwiseService;
use crate::storage::repositories::AnonDmRepository;
use crate::storage::repositories::AnonGroupRepository;
use crate::storage::repositories::AttachmentRepository;
use crate::storage::repositories::SettingsRepository;

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
    /// For checking if invitee has anon DM conversation (to route invites via anon DM tunnel).
    pub(crate) anon_dm_repo: Arc<AnonDmRepository>,
    /// Attachment storage for incoming file metadata.
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    /// For DM-based invites (reaching someone not in group yet).
    pub(crate) message_service: Arc<MessageService>,
    /// Reusable Signal Protocol pairwise encryption.
    pub(crate) pairwise_service: Arc<PairwiseService>,
    /// For sending to anonymous mailboxes.
    pub(crate) mailbox_manager: Arc<MailboxManager>,
    pub(crate) grpc_pool: Arc<GrpcClientPool>,
    pub(crate) token_manager: Arc<TokenManager>,
}

impl AnonGroupService {
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
        }
    }

    /// Generate random padding to reach 160-byte boundary.
    pub(crate) fn generate_padding(boundary: usize) -> Vec<u8> {
        let current_size = 0; // Will be adjusted based on actual content
        let padding_needed = boundary - (current_size % boundary);
        let mut padding = vec![0u8; padding_needed];
        rand::rng().fill_bytes(&mut padding);
        padding
    }
}
