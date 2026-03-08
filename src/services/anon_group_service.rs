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

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use prost::Message as ProstMessage;
use rand::RngCore;
use tokio::time::sleep;
use uuid::Uuid;

use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupMessage as DomainAnonGroupMessage,
    AnonGroupMessageId, AnonGroupPending, AnonGroupRole, PendingActionType,
};
use crate::domain::UserId;
use crate::services::current_user::CurrentUserProvider;
// Note: fanout utilities available in crate::services::fanout for future use
use crate::services::pairwise::PairwiseService;
use crate::error::StorageError;
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::proto::content_v1::{
    self, anon_group_update, AnonDmMessage as AnonDmMessageProto, AnonGroupInvite, AnonGroupJoinAck,
    AnonGroupKick, AnonGroupLeave, AnonGroupMemberInfo, AnonGroupMemberList, AnonGroupMessage,
    AnonGroupUpdate, Content,
};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::{MessageService, SendDmContentParams};
use crate::services::{build_reaction_content, build_typing_content, build_delete_for_everyone_content, build_pin_content, build_edit_content};
use crate::storage::repositories::AnonGroupRepository;
use crate::storage::repositories::AnonDmRepository;
use crate::storage::repositories::AttachmentRepository;
use crate::storage::repositories::SettingsRepository;
use crate::services::AttachmentService;
use crate::util::time::TimestampNanos;

/// Anon group service errors.
#[derive(Debug, thiserror::Error)]
pub enum AnonGroupError {
    /// Not logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// Group not found.
    #[error("group not found: {0}")]
    GroupNotFound(AnonGroupId),

    /// Member not found.
    #[error("member not found: {0}")]
    MemberNotFound(UserId),

    /// Insufficient permissions.
    #[error("insufficient permissions: {0}")]
    InsufficientPermissions(String),

    /// Cannot kick yourself.
    #[error("cannot kick yourself")]
    CannotKickSelf,

    /// Cannot kick the owner.
    #[error("cannot kick the owner")]
    CannotKickOwner,

    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Current user error.
    #[error("current user error: {0}")]
    CurrentUser(#[from] crate::services::current_user::CurrentUserError),

    /// Message send error.
    #[error("message error: {0}")]
    MessageError(String),

    /// Crypto error.
    #[error("crypto error: {0}")]
    CryptoError(String),

    /// Invalid data.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Rate limit exceeded.
    #[error("rate limit exceeded: {0} messages per minute")]
    RateLimitExceeded(u32),

    /// Group is blocked.
    #[error("group is blocked: {0}")]
    GroupBlocked(AnonGroupId),

    /// User is blocked in this group.
    #[error("user is blocked in group: {0}")]
    UserBlocked(UserId),
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
    settings_repo: Arc<SettingsRepository>,
    anon_group_repo: Arc<AnonGroupRepository>,
    /// For checking if invitee has anon DM conversation (to route invites via anon DM tunnel).
    anon_dm_repo: Arc<AnonDmRepository>,
    /// Attachment storage for incoming file metadata.
    attachment_repo: Arc<AttachmentRepository>,
    /// For DM-based invites (reaching someone not in group yet).
    message_service: Arc<MessageService>,
    /// Reusable Signal Protocol pairwise encryption.
    pairwise_service: Arc<PairwiseService>,
    /// For sending to anonymous mailboxes.
    mailbox_manager: Arc<MailboxManager>,
    grpc_pool: Arc<GrpcClientPool>,
    token_manager: Arc<TokenManager>,
}

impl AnonGroupService {
    /// Create a new AnonGroupService.
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

    /// Get the current user ID.
    fn current_user_id(&self) -> Result<UserId, AnonGroupError> {
        Ok(self.settings_repo.current_user_id()?)
    }

    /// Get our profile key if available.
    fn get_my_profile_key(&self) -> Option<[u8; 32]> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_bytes(settings_keys::PROFILE_KEY)
            .ok()
            .flatten()
            .and_then(|v| {
                if v.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&v);
                    Some(arr)
                } else {
                    None
                }
            })
    }

    /// Get our display name from settings.
    fn get_my_display_name(&self) -> Option<String> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_string(settings_keys::DISPLAY_NAME)
            .ok()
            .flatten()
    }

    /// Get our avatar bytes from settings (stored locally).
    fn get_my_avatar(&self) -> Option<Vec<u8>> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()
    }

    // ========================================================================
    // Group Creation
    // ========================================================================

    /// Create a new anonymous group.
    ///
    /// The caller becomes the owner. Invites are sent to the specified users
    /// with staggered delivery to avoid traffic correlation.
    pub async fn create_group(
        &self,
        name: String,
        invitees: Vec<UserId>,
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Create group locally
        let group = AnonGroup::new_as_owner(name);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group.id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as owner member with our mailbox key
        let my_member = AnonGroupMember::with_mailbox(
            group.id,
            current_user,
            AnonGroupRole::Owner,
            mailbox.public_key,
        );
        self.anon_group_repo.add_member(&my_member)?;

        // 4. Send invites with staggered delivery (0-30 seconds random delay)
        let invitee_count = invitees.len();
        for invitee in invitees {
            // Add as pending member
            let member = AnonGroupMember::new(group.id, invitee, AnonGroupRole::Member);
            self.anon_group_repo.add_member(&member)?;

            // Track pending invite
            let pending = AnonGroupPending::new_invite(group.id, invitee);
            self.anon_group_repo.insert_pending(&pending)?;

            // Send invite with small random delay to reduce traffic correlation
            // Keep it short (100-500ms) to maintain good UX
            let delay_ms = 100 + (rand::rng().next_u32() % 400);
            sleep(Duration::from_millis(delay_ms as u64)).await;

            // Check if invitee has an active anon DM conversation with us
            // If so, send invite via anon DM tunnel; otherwise use regular DM
            let send_result = if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_conversation_by_peer(&invitee) {
                if anon_dm.is_ready() {
                    // Send via anon DM tunnel
                    self.send_invite_via_anon_dm(&anon_dm, group.id, &group.name, &mailbox.public_key).await
                } else {
                    // Anon DM not ready, try regular DM
                    self.send_invite_impl(invitee, group.id, &group.name, &mailbox.public_key).await
                }
            } else {
                // No anon DM, use regular DM (friend)
                self.send_invite_impl(invitee, group.id, &group.name, &mailbox.public_key).await
            };

            if let Err(e) = send_result {
                tracing::error!("Failed to send invite to {}: {}", invitee, e);
            }
        }

        tracing::info!(
            "Created anonymous group {} with {} invites",
            group.id,
            invitee_count
        );

        Ok(group)
    }

    /// Create a mailbox for a group using blind signature tokens.
    async fn create_group_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<AnonGroupMailbox, AnonGroupError> {
        // Get or request a blind-signed token for anonymous mailbox registration
        let token = match self.token_manager.get_available_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                // Request new tokens from server
                let mut mailbox_client = MailboxClient::new(
                    self.grpc_pool.channel(),
                    self.grpc_pool.auth_clone(),
                );
                self.token_manager
                    .request_tokens(&mut mailbox_client, 5)
                    .await
                    .map_err(|e| AnonGroupError::CryptoError(e.to_string()))?;

                // Now get a token
                self.token_manager
                    .get_available_token()
                    .map_err(|e| AnonGroupError::CryptoError(e.to_string()))?
                    .ok_or_else(|| AnonGroupError::CryptoError("No tokens available".to_string()))?
            }
            Err(e) => return Err(AnonGroupError::CryptoError(e.to_string())),
        };

        // Use the token's Ed25519 keypair for the mailbox
        let public_key: [u8; 32] = token.public_key.clone().try_into()
            .map_err(|_| AnonGroupError::InvalidData("Invalid public key length".to_string()))?;
        let private_key: [u8; 64] = token.private_key.clone().try_into()
            .map_err(|_| AnonGroupError::InvalidData("Invalid private key length".to_string()))?;

        // Register the mailbox with the server using the blind signature
        let ttl_seconds = 30 * 24 * 60 * 60; // 30 days in seconds
        let mut mailbox_client = MailboxClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        let response = mailbox_client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                ttl_seconds,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| AnonGroupError::MessageError(format!("Failed to create mailbox on server: {}", e)))?;

        tracing::info!(
            group_id = %group_id,
            mailbox_id = %response.mailbox_id,
            expires_at = response.expires_at,
            "Created ephemeral mailbox on server for anonymous group"
        );

        let now = Utc::now();
        let mailbox = AnonGroupMailbox {
            id: Uuid::new_v4().to_string(),
            group_id: *group_id,
            public_key,
            private_key,
            token_id: token.id.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::days(30), // 30 day expiry
        };

        // Mark the token as used
        self.token_manager
            .mark_token_used(&token.id)
            .map_err(|e| AnonGroupError::CryptoError(e.to_string()))?;

        Ok(mailbox)
    }

    // ========================================================================
    // Message Delivery (Separation of Concerns)
    // ========================================================================

    /// Send content via existing DM channel.
    ///
    /// Used for **invites only** since the recipient doesn't have a group mailbox yet.
    /// This goes through DM infrastructure (bootstrap, two-layer encryption, DM mailbox).
    async fn send_via_dm(
        &self,
        peer_id: UserId,
        content: Content,
    ) -> Result<(), AnonGroupError> {
        let mut auth_client = crate::network::grpc::AuthClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        let params = SendDmContentParams {
            auth_client: &mut auth_client,
            peer_id: &peer_id,
            content,
            mailbox_manager: &self.mailbox_manager,
            grpc_pool: Some(&self.grpc_pool),
            attachment_ids: None,
            channel_id: None,
        };

        self.message_service
            .send_dm_content(params)
            .await
            .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;

        Ok(())
    }

    /// Send content to a member's anonymous group mailbox.
    ///
    /// Uses pairwise Signal Protocol encryption (forward secrecy) and delivers
    /// directly to the member's anonymous mailbox. Does NOT use DM infrastructure.
    ///
    /// # Arguments
    /// * `peer_id` - The recipient's user ID (for Signal Protocol session)
    /// * `mailbox_pk` - The recipient's anonymous group mailbox public key
    /// * `content` - The Content proto to send
    async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonGroupError> {
        // 1. Serialize content
        let plaintext = content.encode_to_vec();

        // 2. Encrypt with Signal Protocol (pairwise, no DM-specific layers)
        let ciphertext = self
            .pairwise_service
            .encrypt(peer_id, &plaintext)
            .await
            .map_err(|e| AnonGroupError::CryptoError(e.to_string()))?;

        // 3. Send directly to anonymous mailbox
        self.mailbox_manager
            .send_to_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
            .await
            .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;

        Ok(())
    }

    /// Send an invite message to a user via their existing DM session.
    ///
    /// Invites use DM infrastructure since the recipient doesn't have a
    /// group mailbox yet.
    async fn send_invite_impl(
        &self,
        invitee: UserId,
        group_id: AnonGroupId,
        group_name: &str,
        inviter_mailbox_pk: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        let profile_key = self.get_my_profile_key();
        let mut encrypted_display_name = None;
        let mut encrypted_avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name = crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            if let Some(av) = self.get_my_avatar() {
                encrypted_avatar = crate::crypto::profile_crypto::encrypt_avatar(&pk, &av).ok();
            }
        }

        let invite = AnonGroupInvite {
            group_id: group_id.to_vec(),
            group_name: group_name.to_string(),
            inviter_mailbox_pk: inviter_mailbox_pk.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
            profile_key: profile_key.map(|k| k.to_vec()),
            encrypted_display_name,
            encrypted_avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupInvite(invite)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        // Use DM infrastructure - invitee doesn't have group mailbox yet
        self.send_via_dm(invitee, content).await?;

        tracing::debug!("Sent invite for group {} to {} via regular DM", group_id, invitee);
        Ok(())
    }

    /// Send a group invite via an existing anon DM conversation.
    ///
    /// This routes the invite through the anon DM tunnel instead of regular DM,
    /// preserving anonymity for users who connected via anonymous messaging.
    async fn send_invite_via_anon_dm(
        &self,
        anon_dm: &crate::domain::anon_dm::AnonDm,
        group_id: AnonGroupId,
        group_name: &str,
        inviter_mailbox_pk: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        let peer_mailbox_pk = anon_dm.peer_mailbox_pk.ok_or_else(|| {
            AnonGroupError::InvalidData("Anon DM conversation not ready".to_string())
        })?;

        let profile_key = self.get_my_profile_key();
        let mut encrypted_display_name = None;
        let mut encrypted_avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name = crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            if let Some(av) = self.get_my_avatar() {
                encrypted_avatar = crate::crypto::profile_crypto::encrypt_avatar(&pk, &av).ok();
            }
        }

        let invite = AnonGroupInvite {
            group_id: group_id.to_vec(),
            group_name: group_name.to_string(),
            inviter_mailbox_pk: inviter_mailbox_pk.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
            profile_key: profile_key.map(|k| k.to_vec()),
            encrypted_display_name,
            encrypted_avatar,
        };

        // Wrap invite in Content
        let inner_content = Content {
            content: Some(content_v1::content::Content::AnonGroupInvite(invite)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Vec::new(),
        };

        // Wrap in AnonDmMessage for the anon DM tunnel
        let dm_message = AnonDmMessageProto {
            conversation_id: anon_dm.id.to_vec(),
            inner_content: Some(Box::new(inner_content)),
        };

        let outer_content = Content {
            content: Some(content_v1::content::Content::AnonDmMessage(Box::new(dm_message))),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        // Send to peer's anon DM mailbox
        self.send_to_anon_mailbox(&anon_dm.peer_id, &peer_mailbox_pk, outer_content).await?;

        tracing::debug!(
            "Sent invite for group {} to {} via anon DM tunnel",
            group_id,
            anon_dm.peer_id
        );
        Ok(())
    }

    // ========================================================================
    // Join Flow (Invitee Side)
    // ========================================================================

    /// Accept an invite and join a group.
    ///
    /// Creates our mailbox, stores the group, and sends JoinAck to the inviter.
    pub async fn accept_invite(
        &self,
        group_id: AnonGroupId,
        group_name: String,
        inviter_id: UserId,
        inviter_mailbox_pk: [u8; 32],
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Create group locally as member
        let group = AnonGroup::from_invite(group_id, group_name);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group_id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as member
        let my_member = AnonGroupMember::with_mailbox(
            group_id,
            current_user,
            AnonGroupRole::Member,
            mailbox.public_key,
        );
        self.anon_group_repo.add_member(&my_member)?;

        // 4. Add inviter with their mailbox key
        let inviter_member = AnonGroupMember::with_mailbox(
            group_id,
            inviter_id,
            AnonGroupRole::Owner, // Assume owner until we get member list
            inviter_mailbox_pk,
        );
        self.anon_group_repo.add_member(&inviter_member)?;

        // 5. Send JoinAck to inviter's anonymous mailbox
        self.send_join_ack(&inviter_id, group_id, &inviter_mailbox_pk, &mailbox.public_key)
            .await?;

        tracing::info!("Joined anonymous group {} from inviter {}", group_id, inviter_id);
        Ok(group)
    }

    /// Send a JoinAck message to the inviter's anonymous group mailbox.
    ///
    /// Uses pairwise encryption (Signal Protocol) and delivers directly to the
    /// inviter's anonymous mailbox - NOT through DM infrastructure.
    async fn send_join_ack(
        &self,
        inviter_id: &UserId,
        group_id: AnonGroupId,
        inviter_mailbox_pk: &[u8; 32],
        my_mailbox_pk: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        let profile_key = self.get_my_profile_key();
        let mut encrypted_display_name = None;
        let mut encrypted_avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name = crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            if let Some(av) = self.get_my_avatar() {
                encrypted_avatar = crate::crypto::profile_crypto::encrypt_avatar(&pk, &av).ok();
            }
        }

        let ack = AnonGroupJoinAck {
            group_id: group_id.to_vec(),
            joiner_mailbox_pk: my_mailbox_pk.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
            profile_key: profile_key.map(|k| k.to_vec()),
            encrypted_display_name,
            encrypted_avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupJoinAck(ack)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        // Send to inviter's anonymous group mailbox (not DM)
        self.send_to_anon_mailbox(inviter_id, inviter_mailbox_pk, content).await?;

        tracing::debug!("Sent JoinAck for group {} to {}", group_id, inviter_id);
        Ok(())
    }

    // ========================================================================
    // Join Flow (Initiator Side)
    // ========================================================================

    /// Handle a JoinAck from an invitee.
    ///
    /// Updates the member's mailbox key and broadcasts updated member list.
    pub async fn handle_join_ack(
        &self,
        group_id: AnonGroupId,
        joiner_id: UserId,
        joiner_mailbox_pk: [u8; 32],
    ) -> Result<(), AnonGroupError> {
        // 1. Verify the group exists and we're the owner/admin
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        if !group.is_admin_or_owner() {
            // Not an error - could be a late ack received as non-owner
            tracing::debug!("Received JoinAck but not owner/admin for group {}", group_id);
        }

        // 2. Update member's mailbox key
        self.anon_group_repo
            .update_member_mailbox(&group_id, &joiner_id, &joiner_mailbox_pk)?;

        // 3. Remove pending invite
        self.anon_group_repo
            .delete_pending(&group_id, &joiner_id, PendingActionType::Invite)?;

        // 4. Broadcast updated member list to all confirmed members
        if group.is_owner() {
            self.broadcast_member_list(&group_id).await?;
        }

        tracing::info!(
            "Member {} joined group {} with mailbox key",
            joiner_id,
            group_id
        );
        Ok(())
    }

    /// Broadcast the current member list to all confirmed members.
    async fn broadcast_member_list(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let members = self.anon_group_repo.get_members(group_id)?;
        let confirmed_members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Build member info list
        let member_infos: Vec<AnonGroupMemberInfo> = members
            .iter()
            .map(|m| {
                let profile_key = m.profile_key.map(|k| k.to_vec());
                let mut encrypted_display_name = None;
                let mut encrypted_avatar = None;

                if let Some(pk) = m.profile_key {
                    if let Some(ref name) = m.display_name {
                        if !name.is_empty() {
                            encrypted_display_name = crate::crypto::profile_crypto::encrypt_display_name(&pk, name).ok();
                        }
                    }
                    if let Some(ref av) = m.avatar {
                        encrypted_avatar = crate::crypto::profile_crypto::encrypt_avatar(&pk, av).ok();
                    }
                }

                AnonGroupMemberInfo {
                    user_id: m.user_id.as_bytes().to_vec(),
                    mailbox_pk: m.mailbox_pk.map(|pk| pk.to_vec()).unwrap_or_default(),
                    role: m.role.to_i32(),
                    profile_key,
                    encrypted_display_name,
                    encrypted_avatar,
                }
            })
            .collect();

        let member_list = AnonGroupMemberList {
            group_id: group_id.to_vec(),
            members: member_infos,
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupMemberList(
                member_list,
            )),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        let current_user = self.current_user_id()?;

        let member_count = confirmed_members.len();
        // Send to each confirmed member's anonymous mailbox (staggered)
        for member in &confirmed_members {
            if member.user_id == current_user {
                continue; // Don't send to ourselves
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member.mailbox_pk.expect("get_members_with_mailbox guarantees mailbox_pk");

            let delay_ms = rand::rng().next_u32() % 5_000; // 0-5 second delay
            sleep(Duration::from_millis(delay_ms as u64)).await;

            if let Err(e) = self.send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone()).await {
                tracing::error!("Failed to send member list to {}: {}", member.user_id, e);
            }
        }

        tracing::debug!(
            "Broadcasting member list for group {} to {} members",
            group_id,
            member_count.saturating_sub(1)
        );
        Ok(())
    }

    // ========================================================================
    // Member List Updates
    // ========================================================================

    /// Handle an incoming member list update.
    pub fn handle_member_list(
        &self,
        group_id: AnonGroupId,
        members: Vec<AnonGroupMemberInfo>,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Update local member state
        for member_info in members {
            let user_id = UserId::try_from_slice(&member_info.user_id)
                .ok_or_else(|| AnonGroupError::InvalidData("Invalid user ID".to_string()))?;

            // Skip ourselves
            if user_id == current_user {
                continue;
            }

            let role = AnonGroupRole::from_i32(member_info.role);
            let mailbox_pk = if member_info.mailbox_pk.len() == 32 {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&member_info.mailbox_pk);
                Some(pk)
            } else {
                None
            };

            let profile_key = if let Some(ref pk) = member_info.profile_key {
                if pk.len() == 32 {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(pk);
                    Some(key)
                } else {
                    None
                }
            } else {
                None
            };

            use crate::crypto::profile_crypto::{decrypt_display_name, decrypt_avatar};

            let display_name: Option<String> = match (&profile_key, &member_info.encrypted_display_name) {
                (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                    match decrypt_display_name(pk, encrypted) {
                        Ok(name) if !name.is_empty() => Some(name),
                        _ => None,
                    }
                }
                _ => None,
            };

            let avatar: Option<Vec<u8>> = match (&profile_key, &member_info.encrypted_avatar) {
                (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                    match decrypt_avatar(pk, encrypted) {
                        Ok(av) => Some(av),
                        _ => None,
                    }
                }
                _ => None,
            };

            // Check if member already exists
            if self.anon_group_repo.is_member(&group_id, &user_id)? {
                // Update existing member
                self.anon_group_repo.update_member_role(&group_id, &user_id, role)?;
                if let Some(pk) = mailbox_pk {
                    self.anon_group_repo
                        .update_member_mailbox_and_profile(&group_id, &user_id, &pk, profile_key, display_name, avatar)?;
                }
            } else {
                // Add new member
                let member = if let Some(pk) = mailbox_pk {
                    AnonGroupMember::with_mailbox(group_id, user_id, role, pk).with_profile(profile_key, display_name, avatar)
                } else {
                    AnonGroupMember::new(group_id, user_id, role).with_profile(profile_key, display_name, avatar)
                };
                self.anon_group_repo.add_member(&member)?;
            }
        }

        tracing::debug!("Updated member list for group {}", group_id);
        Ok(())
    }

    // ========================================================================
    // Messaging
    // ========================================================================

    /// Send a text message to all members of an anonymous group.
    ///
    /// This is a convenience wrapper around `send_message` that creates a
    /// DataMessage Content with the given text.
    ///
    /// Returns the stored message ID.
    pub async fn send_text(
        &self,
        group_id: &AnonGroupId,
        text: &str,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        use crate::proto::content_v1::DataMessage;

        let current_user = self.current_user_id()?;

        // Create timestamp once - used for both local storage and sending
        let sent_at = TimestampNanos::now().as_u64();

        // Create DataMessage content
        let data_message = DataMessage {
            body: text.to_string(),
            ..Default::default()
        };
        let inner_content = Content {
            content: Some(content_v1::content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: sent_at,
            padding: Vec::new(),
        };

        // Store message locally first with the same timestamp we'll send
        let message_id = self.store_text_message(group_id, &current_user, text, sent_at)?;

        // Send to all members
        self.send_message(group_id, inner_content).await?;

        Ok(message_id)
    }

    /// Send a text message with attachments to an anonymous group.
    ///
    /// Similar to `send_text` but also includes attachment pointers.
    /// Attachments should already be uploaded to CDN before calling this.
    ///
    /// Returns the stored message ID.
    pub async fn send_with_attachments(
        &self,
        group_id: &AnonGroupId,
        text: &str,
        attachments: Vec<crate::proto::content_v1::AttachmentPointer>,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        use crate::domain::AttachmentId;
        use crate::proto::content_v1::DataMessage;

        let current_user = self.current_user_id()?;

        // Extract attachment IDs from pointers before moving them
        let attachment_ids: Vec<AttachmentId> = attachments
            .iter()
            .filter_map(|ptr| {
                if ptr.cdn_key.is_empty() {
                    None
                } else {
                    Some(AttachmentId::from_cdn_key(&ptr.cdn_key))
                }
            })
            .collect();

        // Create timestamp once - used for both local storage and sending
        let sent_at = TimestampNanos::now().as_u64();

        // Create DataMessage content with attachments
        let data_message = DataMessage {
            body: text.to_string(),
            attachments,
            ..Default::default()
        };
        let inner_content = Content {
            content: Some(content_v1::content::Content::DataMessage(data_message)),
            ephemeral_reply_to: None,
            timestamp: sent_at,
            padding: Vec::new(),
        };

        // Store message locally first with the same timestamp we'll send
        let message_id = self.store_message_with_attachments(
            group_id,
            &current_user,
            text,
            &attachment_ids,
            sent_at,
        )?;

        // Send to all members
        self.send_message(group_id, inner_content).await?;

        tracing::info!(
            group_id = %group_id,
            text_len = text.len(),
            attachment_count = attachment_ids.len(),
            "Sent group message with attachments"
        );

        Ok(message_id)
    }

    /// Send a message to all members of an anonymous group.
    ///
    /// The message is wrapped in AnonGroupMessage and sent to each member's
    /// anonymous mailbox via pairwise sealed sender.
    ///
    /// # Errors
    ///
    /// - `GroupBlocked` if the group is blocked
    /// - `RateLimitExceeded` if too many messages sent recently
    pub async fn send_message(
        &self,
        group_id: &AnonGroupId,
        inner_content: Content,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Check if group is blocked (cannot send to blocked groups)
        self.check_group_not_blocked(group_id)?;

        // Check rate limit before sending
        self.check_rate_limit(group_id)?;

        // Verify group exists and we're a member
        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        // Get confirmed members (those with mailbox keys)
        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Wrap the content in AnonGroupMessage
        let group_message = AnonGroupMessage {
            group_id: group_id.to_vec(),
            inner_content: Some(Box::new(inner_content)),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(group_message))),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        let mut sent_count = 0;
        // Fan out to each member's anonymous mailbox
        for member in &members {
            if member.user_id == current_user {
                continue; // Don't send to ourselves
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member.mailbox_pk.expect("get_members_with_mailbox guarantees mailbox_pk");

            // Send to member's anonymous group mailbox (not DM)
            if let Err(e) = self.send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone()).await {
                tracing::warn!("Failed to send to member {}: {}", member.user_id, e);
            } else {
                sent_count += 1;
            }
        }

        tracing::debug!("Sent message to group {} ({} members)", group_id, sent_count);
        Ok(())
    }

    // ========================================================================
    // Ephemeral Messages (Reactions, Typing, Delete, Pin)
    // ========================================================================

    /// Send a reaction to a message in the group.
    pub async fn send_reaction(
        &self,
        group_id: &AnonGroupId,
        emoji: &str,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_reaction_content(emoji, remove, target_author_aci, target_sent_timestamp);
        self.send_message(group_id, content).await
    }

    /// Send a typing indicator to all group members.
    pub async fn send_typing_indicator(
        &self,
        group_id: &AnonGroupId,
        started: bool,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_typing_content(started);
        self.send_message(group_id, content).await
    }

    /// Send delete-for-everyone to all group members.
    pub async fn send_delete_for_everyone(
        &self,
        group_id: &AnonGroupId,
        target_sent_timestamp: u64,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_delete_for_everyone_content(target_sent_timestamp);
        self.send_message(group_id, content).await
    }

    /// Send pin/unpin notification to all group members.
    pub async fn send_pin(
        &self,
        group_id: &AnonGroupId,
        target_author_aci: &str,
        target_sent_timestamp: u64,
        pin: bool,
    ) -> Result<(), AnonGroupError> {
        // Silently ignore if blocked
        if self.check_group_not_blocked(group_id).is_err() {
            return Ok(());
        }

        let content = build_pin_content(target_author_aci, target_sent_timestamp, pin);
        self.send_message(group_id, content).await
    }

    /// Send an edit message to update a previously sent message.
    ///
    /// This sends an edit notification to all group members and updates the message locally.
    pub async fn send_edit(
        &self,
        group_id: &AnonGroupId,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> Result<(), AnonGroupError> {
        let content = build_edit_content(target_sent_timestamp, new_text);
        self.send_message(group_id, content).await?;

        // Update message locally
        self.anon_group_repo.update_message_text(group_id, target_sent_timestamp, new_text)?;

        Ok(())
    }

    // ========================================================================
    // Leave / Kick
    // ========================================================================

    /// Leave a group voluntarily.
    pub async fn leave_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        // Get confirmed members before we remove ourselves
        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Build leave message
        let leave = AnonGroupLeave {
            group_id: group_id.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupLeave(leave)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        // Notify other members via their anonymous mailboxes
        for member in members {
            if member.user_id == current_user {
                continue;
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member.mailbox_pk.expect("get_members_with_mailbox guarantees mailbox_pk");

            if let Err(e) = self.send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone()).await {
                tracing::warn!("Failed to notify {} of leave: {}", member.user_id, e);
            }
        }

        // Delete local group data
        self.anon_group_repo.delete_group(group_id)?;

        tracing::info!("Left group {}", group_id);
        Ok(())
    }

    /// Kick a member from the group (admin/owner only).
    pub async fn kick_member(
        &self,
        group_id: &AnonGroupId,
        target_user_id: &UserId,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Can't kick yourself
        if *target_user_id == current_user {
            return Err(AnonGroupError::CannotKickSelf);
        }

        // Verify group exists and we have permission
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if !group.my_role.can_kick() {
            return Err(AnonGroupError::InsufficientPermissions(
                "Only admins can kick members".to_string(),
            ));
        }

        // Can't kick the owner
        let target_role = self
            .anon_group_repo
            .get_member_role(group_id, target_user_id)?
            .ok_or(AnonGroupError::MemberNotFound(*target_user_id))?;

        if target_role == AnonGroupRole::Owner {
            return Err(AnonGroupError::CannotKickOwner);
        }

        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Build kick message with signature
        let timestamp = TimestampNanos::now().as_u64();
        let admin_signature = self.sign_kick(group_id, target_user_id, timestamp)?;

        let kick = AnonGroupKick {
            group_id: group_id.to_vec(),
            kicked_user_id: target_user_id.as_bytes().to_vec(),
            timestamp,
            admin_signature,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupKick(kick)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(160),
        };

        // Notify all members except the kicked user (via anonymous mailboxes)
        for member in members {
            if member.user_id == current_user || member.user_id == *target_user_id {
                continue;
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member.mailbox_pk.expect("get_members_with_mailbox guarantees mailbox_pk");

            if let Err(e) = self.send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone()).await {
                tracing::warn!("Failed to notify {} of kick: {}", member.user_id, e);
            }
        }

        // Remove from local member list
        self.anon_group_repo.remove_member(group_id, target_user_id)?;

        tracing::info!("Kicked {} from group {}", target_user_id, group_id);
        Ok(())
    }

    /// Sign a kick message using our group mailbox private key.
    ///
    /// Uses the Ed25519 mailbox key for this group to sign the kick message.
    /// Recipients can verify using the sender's mailbox public key.
    fn sign_kick(
        &self,
        group_id: &AnonGroupId,
        target_user_id: &UserId,
        timestamp: u64,
    ) -> Result<Vec<u8>, AnonGroupError> {
        use ed25519_dalek::{Signer, SigningKey};

        // Build message to sign: group_id || kicked_user_id || timestamp
        let mut message = Vec::with_capacity(16 + 16 + 8);
        message.extend_from_slice(group_id.as_bytes());
        message.extend_from_slice(target_user_id.as_bytes());
        message.extend_from_slice(&timestamp.to_le_bytes());

        // Get our mailbox for this group
        let mailbox = self
            .anon_group_repo
            .get_mailbox(group_id)?
            .ok_or_else(|| AnonGroupError::CryptoError("No mailbox for group".to_string()))?;

        // Extract the Ed25519 signing key (first 32 bytes of the 64-byte keypair)
        let signing_key = SigningKey::from_bytes(&mailbox.private_key[..32].try_into()
            .map_err(|_| AnonGroupError::CryptoError("Invalid private key".to_string()))?);

        // Sign the message
        let signature = signing_key.sign(&message);

        Ok(signature.to_bytes().to_vec())
    }

    /// Verify a kick signature using the sender's mailbox public key.
    fn verify_kick_signature(
        &self,
        group_id: &AnonGroupId,
        kicked_user_id: &UserId,
        timestamp: u64,
        sender_id: &UserId,
        signature: &[u8],
    ) -> Result<bool, AnonGroupError> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        // Build message that was signed
        let mut message = Vec::with_capacity(16 + 16 + 8);
        message.extend_from_slice(group_id.as_bytes());
        message.extend_from_slice(kicked_user_id.as_bytes());
        message.extend_from_slice(&timestamp.to_le_bytes());

        // Get sender's mailbox public key
        let members = self.anon_group_repo.get_members(group_id)?;
        let sender_mailbox_pk = members
            .iter()
            .find(|m| m.user_id == *sender_id)
            .and_then(|m| m.mailbox_pk)
            .ok_or_else(|| AnonGroupError::MemberNotFound(*sender_id))?;

        // Parse the signature
        if signature.len() != 64 {
            return Ok(false);
        }
        let sig_bytes: [u8; 64] = signature.try_into()
            .map_err(|_| AnonGroupError::CryptoError("Invalid signature length".to_string()))?;
        let sig = Signature::from_bytes(&sig_bytes);

        // Parse the verifying key
        let verifying_key = VerifyingKey::from_bytes(&sender_mailbox_pk)
            .map_err(|e| AnonGroupError::CryptoError(format!("Invalid public key: {}", e)))?;

        // Verify
        Ok(verifying_key.verify(&message, &sig).is_ok())
    }

    // ========================================================================
    // Handle Incoming Messages
    // ========================================================================

    /// Handle an incoming leave message.
    pub fn handle_leave(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
    ) -> Result<(), AnonGroupError> {
        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Verify sender is a member
        if !self.anon_group_repo.is_member(&group_id, &sender_id)? {
            tracing::debug!("Leave from non-member {} in group {}", sender_id, group_id);
            return Ok(());
        }

        // Remove member
        self.anon_group_repo.remove_member(&group_id, &sender_id)?;

        tracing::info!("Member {} left group {}", sender_id, group_id);
        Ok(())
    }

    /// Handle an incoming kick message.
    pub fn handle_kick(
        &self,
        group_id: AnonGroupId,
        kicked_user_id: UserId,
        sender_id: UserId,
        admin_signature: &[u8],
        timestamp: u64,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Verify sender has kick permission
        let sender_role = self
            .anon_group_repo
            .get_member_role(&group_id, &sender_id)?
            .ok_or(AnonGroupError::MemberNotFound(sender_id))?;

        if !sender_role.can_kick() {
            tracing::warn!(
                "Kick from non-admin {} in group {} - ignoring",
                sender_id,
                group_id
            );
            return Ok(());
        }

        // Verify the signature using sender's mailbox public key
        if !admin_signature.is_empty() {
            match self.verify_kick_signature(&group_id, &kicked_user_id, timestamp, &sender_id, admin_signature) {
                Ok(true) => {
                    tracing::debug!("Kick signature verified for group {}", group_id);
                }
                Ok(false) => {
                    tracing::warn!(
                        "Invalid kick signature from {} in group {} - ignoring",
                        sender_id,
                        group_id
                    );
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("Failed to verify kick signature: {} - processing anyway", e);
                    // Continue processing - signature verification failure shouldn't block kick
                    // This allows graceful degradation for older clients
                }
            }
        }

        // If we're being kicked, leave the group
        if kicked_user_id == current_user {
            self.anon_group_repo.delete_group(&group_id)?;
            tracing::info!("We were kicked from group {}", group_id);
            return Ok(());
        }

        // Remove the kicked member from our local list
        if self.anon_group_repo.is_member(&group_id, &kicked_user_id)? {
            self.anon_group_repo.remove_member(&group_id, &kicked_user_id)?;
        }

        tracing::info!(
            "Member {} was kicked from group {} by {}",
            kicked_user_id,
            group_id,
            sender_id
        );
        Ok(())
    }

    /// Handle an incoming group update message (name change, role change).
    pub fn handle_update(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        update: &AnonGroupUpdate,
    ) -> Result<(), AnonGroupError> {
        // Verify group exists
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Verify sender is a member with appropriate permissions
        let sender_role = self
            .anon_group_repo
            .get_member_role(&group_id, &sender_id)?
            .ok_or(AnonGroupError::MemberNotFound(sender_id))?;

        match &update.update {
            Some(anon_group_update::Update::NewName(new_name)) => {
                // Only admins/owners can rename
                if !sender_role.can_kick() {
                    tracing::warn!(
                        "Name change from non-admin {} in group {} - ignoring",
                        sender_id,
                        group_id
                    );
                    return Ok(());
                }

                self.anon_group_repo.update_group_name(&group_id, new_name)?;
                tracing::info!(
                    "Group {} renamed to '{}' by {}",
                    group_id,
                    new_name,
                    sender_id
                );
            }
            Some(anon_group_update::Update::RoleChange(role_change)) => {
                // Only owners can change roles
                if !group.is_owner() {
                    tracing::debug!(
                        "Role change from non-owner in group {} - ignoring",
                        group_id
                    );
                    return Ok(());
                }

                let target_user_id = UserId::try_from_slice(&role_change.user_id)
                    .ok_or_else(|| AnonGroupError::InvalidData("Invalid user ID in role change".to_string()))?;

                let new_role = AnonGroupRole::from_i32(role_change.new_role);

                self.anon_group_repo.update_member_role(&group_id, &target_user_id, new_role)?;
                tracing::info!(
                    "Member {} role changed to {:?} in group {} by {}",
                    target_user_id,
                    new_role,
                    group_id,
                    sender_id
                );
            }
            None => {
                tracing::debug!("Empty update for group {} - ignoring", group_id);
            }
        }

        Ok(())
    }

    // ========================================================================
    // Queries
    // ========================================================================

    /// Get a group by ID.
    pub fn get_group(&self, group_id: &AnonGroupId) -> Result<Option<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.get_group(group_id)?)
    }

    /// List all groups.
    pub fn list_groups(&self) -> Result<Vec<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.list_groups()?)
    }

    /// Get members of a group.
    pub fn get_members(&self, group_id: &AnonGroupId) -> Result<Vec<AnonGroupMember>, AnonGroupError> {
        Ok(self.anon_group_repo.get_members(group_id)?)
    }

    /// Get a member's display name from their group profile.
    pub fn get_member_display_name(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<Option<String>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member_display_name(group_id, user_id)?)
    }

    /// Check if a user is a member of a group.
    pub fn is_member(&self, group_id: &AnonGroupId, user_id: &UserId) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.is_member(group_id, user_id)?)
    }

    /// Update a member's mailbox public key and profile (sync, no encryption).
    ///
    /// Called by the handler when a JoinAck is received.
    pub fn update_member_mailbox_and_profile(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo.update_member_mailbox_and_profile(group_id, user_id, &mailbox_pk, profile_key, display_name, avatar)?;
        // Also remove pending invite
        let _ = self.anon_group_repo.delete_pending(group_id, user_id, PendingActionType::Invite);
        Ok(())
    }

    // ========================================================================
    // Message Storage
    // ========================================================================

    /// Store a message received from a group member.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_message(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let message = DomainAnonGroupMessage {
            id: AnonGroupMessageId::new(),
            group_id: *group_id,
            sender_id: *sender_id,
            content_type,
            content_json: content_json.to_string(),
            attachments_blob: vec![8, 0, 0, 0, 0, 0, 0, 0], // Empty vec bincode
            is_deleted: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
        };

        self.anon_group_repo.insert_message(&message)?;
        tracing::debug!("Stored message {} in group {} with sent_at={}", message.id, group_id, sent_at);
        Ok(message.id)
    }

    /// Store a text message in a group.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_text_message(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        text: &str,
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        let message = DomainAnonGroupMessage::new_text(*group_id, *sender_id, text, sent_at);
        self.anon_group_repo.insert_message(&message)?;
        tracing::debug!("Stored text message {} in group {} with sent_at={}", message.id, group_id, sent_at);
        Ok(message.id)
    }

    /// Store a message with text and/or attachments.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for consistent ordering across devices.
    pub fn store_message_with_attachments(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        text: &str,
        attachment_ids: &[crate::domain::AttachmentId],
        sent_at: u64,
    ) -> Result<AnonGroupMessageId, AnonGroupError> {
        use crate::storage::serialization::db_serialize;

        // Determine content type: 1=text, 2=attachment only, 3=mixed
        let has_text = !text.is_empty();
        let has_attachments = !attachment_ids.is_empty();

        let (content_type, content_json) = match (has_text, has_attachments) {
            (true, false) => (1, serde_json::json!({ "text": text }).to_string()),
            (false, true) => (2, serde_json::json!({ "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }).to_string()),
            (true, true) => (3, serde_json::json!({ "text": text, "attachments": attachment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }).to_string()),
            (false, false) => (0, "{}".to_string()),
        };

        // Serialize attachment IDs to blob
        let attachment_ids_vec: Vec<_> = attachment_ids.to_vec();
        let attachments_blob = db_serialize(&attachment_ids_vec).unwrap_or_else(|_| vec![8, 0, 0, 0, 0, 0, 0, 0]);

        let message = DomainAnonGroupMessage {
            id: AnonGroupMessageId::new(),
            group_id: *group_id,
            sender_id: *sender_id,
            content_type,
            content_json,
            attachments_blob,
            is_deleted: false,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            delivery_status: String::new(),
            sent_at,
            created_at: Utc::now(),
        };

        self.anon_group_repo.insert_message(&message)?;
        tracing::debug!(
            "Stored message {} in group {} with sent_at={}, attachments={}",
            message.id,
            group_id,
            sent_at,
            attachment_ids.len()
        );
        Ok(message.id)
    }

    /// Create attachment records from pointers.
    ///
    /// Returns the list of created attachment IDs for storing in the message.
    pub fn create_attachments_from_pointers(
        &self,
        pointers: &[crate::proto::content_v1::AttachmentPointer],
    ) -> Vec<crate::domain::AttachmentId> {
        let attachment_service = AttachmentService::new(self.attachment_repo.clone());
        attachment_service.create_from_pointers(pointers)
    }

    /// Get a message by sender ID and timestamp (Signal-style lookup).
    ///
    /// This is used for context menu operations, reactions, delete, pin, etc.
    /// Messages are identified by (sender_id, sent_at) tuple per Signal Protocol.
    pub fn get_message_by_sender_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_message_by_sender_timestamp(group_id, sender_id, timestamp_ns)?)
    }

    /// Delete a message by sender timestamp (for delete-for-everyone).
    pub fn delete_message_by_timestamp(
        &self,
        group_id: &AnonGroupId,
        sender_id: &UserId,
        timestamp_ns: u64,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.mark_message_deleted_by_timestamp(group_id, sender_id, timestamp_ns)?)
    }

    /// Get messages for a group with pagination.
    ///
    /// Returns messages ordered by created_at DESC (newest first).
    pub fn get_messages(
        &self,
        group_id: &AnonGroupId,
        before: Option<&chrono::DateTime<Utc>>,
        limit: i32,
    ) -> Result<Vec<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_messages(group_id, before, limit)?)
    }

    /// Get a single message by ID.
    pub fn get_message(
        &self,
        id: &AnonGroupMessageId,
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_message(id)?)
    }

    /// Get the most recent message in a group (for sidebar preview).
    pub fn get_latest_message(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<DomainAnonGroupMessage>, AnonGroupError> {
        Ok(self.anon_group_repo.get_latest_message(group_id)?)
    }

    /// Get the message count for a group.
    pub fn get_message_count(&self, group_id: &AnonGroupId) -> Result<i64, AnonGroupError> {
        Ok(self.anon_group_repo.get_message_count(group_id)?)
    }

    /// Delete a message (soft delete).
    pub fn delete_message(&self, id: &AnonGroupMessageId) -> Result<(), AnonGroupError> {
        self.anon_group_repo.delete_message(id)?;
        Ok(())
    }

    // ========================================================================
    // Blocking / Muting
    // ========================================================================

    /// Block a group (ignore all messages, stop polling mailbox).
    pub fn block_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        use crate::domain::anon_group::AnonGroupStatus;
        self.anon_group_repo.update_status(group_id, AnonGroupStatus::Blocked)?;
        tracing::info!("Blocked group {}", group_id);
        Ok(())
    }

    /// Unblock a group (resume normal operation).
    pub fn unblock_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        use crate::domain::anon_group::AnonGroupStatus;
        self.anon_group_repo.update_status(group_id, AnonGroupStatus::Active)?;
        tracing::info!("Unblocked group {}", group_id);
        Ok(())
    }

    /// Mute a group (receive messages but no notifications).
    pub fn mute_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        use crate::domain::anon_group::AnonGroupStatus;
        self.anon_group_repo.update_status(group_id, AnonGroupStatus::Muted)?;
        tracing::info!("Muted group {}", group_id);
        Ok(())
    }

    /// Unmute a group (resume notifications).
    pub fn unmute_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        use crate::domain::anon_group::AnonGroupStatus;
        self.anon_group_repo.update_status(group_id, AnonGroupStatus::Active)?;
        tracing::info!("Unmuted group {}", group_id);
        Ok(())
    }

    /// Check if a group is blocked.
    pub fn is_group_blocked(&self, group_id: &AnonGroupId) -> Result<bool, AnonGroupError> {
        match self.anon_group_repo.get_group(group_id)? {
            Some(group) => Ok(group.is_blocked()),
            None => Err(AnonGroupError::GroupNotFound(*group_id)),
        }
    }

    /// List only active (non-blocked) groups.
    pub fn list_active_groups(&self) -> Result<Vec<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.list_active_groups()?)
    }

    // ========================================================================
    // Per-User Blocking within Groups
    // ========================================================================

    /// Block a user within a group (ignore their messages).
    pub fn block_user_in_group(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), AnonGroupError> {
        // Verify group exists
        let _ = self.anon_group_repo.get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        self.anon_group_repo.block_user(group_id, user_id, reason)?;
        tracing::info!("Blocked user {} in group {}", user_id, group_id);
        Ok(())
    }

    /// Unblock a user within a group.
    pub fn unblock_user_in_group(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo.unblock_user(group_id, user_id)?;
        tracing::info!("Unblocked user {} in group {}", user_id, group_id);
        Ok(())
    }

    /// Check if a user is blocked within a group.
    pub fn is_user_blocked_in_group(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.is_user_blocked(group_id, user_id)?)
    }

    /// Get all blocked users in a group.
    pub fn get_blocked_users(&self, group_id: &AnonGroupId) -> Result<Vec<UserId>, AnonGroupError> {
        Ok(self.anon_group_repo.get_blocked_users(group_id)?)
    }

    // ========================================================================
    // Rate Limiting
    // ========================================================================

    /// Default rate limit: 60 messages per minute per group.
    const RATE_LIMIT_MESSAGES: u32 = 60;
    /// Default rate limit window: 1 minute.
    const RATE_LIMIT_WINDOW: chrono::Duration = chrono::Duration::minutes(1);

    /// Check if sending a message would exceed rate limits.
    ///
    /// Returns Ok(()) if allowed, Err if rate limited.
    pub fn check_rate_limit(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let count = self.anon_group_repo.increment_rate_limit(group_id, Self::RATE_LIMIT_WINDOW)?;

        if count > Self::RATE_LIMIT_MESSAGES {
            tracing::warn!(
                "Rate limit exceeded for group {}: {} messages in window",
                group_id,
                count
            );
            return Err(AnonGroupError::RateLimitExceeded(Self::RATE_LIMIT_MESSAGES));
        }

        Ok(())
    }

    /// Check if the group is blocked (cannot send/receive).
    pub fn check_group_not_blocked(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if group.is_blocked() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }

        Ok(())
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    /// Generate random padding to reach 160-byte boundary.
    fn generate_padding(boundary: usize) -> Vec<u8> {
        let current_size = 0; // Will be adjusted based on actual content
        let padding_needed = boundary - (current_size % boundary);
        let mut padding = vec![0u8; padding_needed];
        rand::rng().fill_bytes(&mut padding);
        padding
    }
}
