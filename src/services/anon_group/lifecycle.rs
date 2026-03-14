//! Group creation and join flow operations.

use std::time::Duration;

use chrono::Utc;
use rand::RngCore;
use tokio::time::sleep;
use uuid::Uuid;

use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupPending, AnonGroupRole,
};
use crate::domain::UserId;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::settings_keys;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Get the current user ID.
    pub fn current_user_id(&self) -> Result<UserId, AnonGroupError> {
        Ok(self.settings_repo.current_user_id()?)
    }

    /// Get our profile key if available.
    pub(crate) fn get_my_profile_key(&self) -> Option<[u8; 32]> {
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
    pub(crate) fn get_my_display_name(&self) -> Option<String> {
        self.settings_repo
            .get_string(settings_keys::DISPLAY_NAME)
            .ok()
            .flatten()
    }

    /// Get our avatar bytes from settings (stored locally).
    pub(crate) fn get_my_avatar(&self) -> Option<Vec<u8>> {
        self.settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()
    }

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
        let group = AnonGroup::new_as_owner(name, current_user);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group.id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as owner member with our mailbox key and profile
        let profile_key = self.get_my_profile_key();
        let my_display_name = self.get_my_display_name();
        let my_avatar = self.get_my_avatar();
        let my_member = AnonGroupMember::with_mailbox(
            group.id,
            current_user,
            AnonGroupRole::Owner,
            mailbox.public_key,
        )
        .with_profile(profile_key, my_display_name, my_avatar);
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
            let send_result =
                if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_conversation_by_peer(&invitee) {
                    if anon_dm.is_ready() {
                        // Send via anon DM tunnel
                        self.send_invite_via_anon_dm(
                            &anon_dm,
                            group.id,
                            &group.name,
                            &mailbox.public_key,
                        )
                        .await
                    } else {
                        // Anon DM not ready, try regular DM
                        self.send_invite_impl(invitee, group.id, &group.name, &mailbox.public_key)
                            .await
                    }
                } else {
                    // No anon DM, use regular DM (friend)
                    self.send_invite_impl(invitee, group.id, &group.name, &mailbox.public_key)
                        .await
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

    /// Create a new anonymous group with specified roles for invitees.
    ///
    /// The caller becomes the owner. Each invitee is assigned the specified role
    /// (Admin or Member). Invites are sent with staggered delivery.
    pub async fn create_group_with_roles(
        &self,
        name: String,
        invitees: Vec<(UserId, AnonGroupRole)>,
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Create group locally
        let group = AnonGroup::new_as_owner(name, current_user);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group.id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as owner member with our mailbox key and profile
        let profile_key = self.get_my_profile_key();
        let my_display_name = self.get_my_display_name();
        let my_avatar = self.get_my_avatar();
        let my_member = AnonGroupMember::with_mailbox(
            group.id,
            current_user,
            AnonGroupRole::Owner,
            mailbox.public_key,
        )
        .with_profile(profile_key, my_display_name, my_avatar);
        self.anon_group_repo.add_member(&my_member)?;

        // 4. Send invites with staggered delivery
        let invitee_count = invitees.len();
        for (invitee, role) in invitees {
            // Add as pending member with specified role
            let member = AnonGroupMember::new(group.id, invitee, role);
            self.anon_group_repo.add_member(&member)?;

            // Track pending invite
            let pending = AnonGroupPending::new_invite(group.id, invitee);
            self.anon_group_repo.insert_pending(&pending)?;

            // Send invite with small random delay to reduce traffic correlation
            let delay_ms = 100 + (rand::rng().next_u32() % 400);
            sleep(Duration::from_millis(delay_ms as u64)).await;

            // Check if invitee has an active anon DM conversation with us
            let send_result =
                if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_conversation_by_peer(&invitee) {
                    if anon_dm.is_ready() {
                        self.send_invite_via_anon_dm(
                            &anon_dm,
                            group.id,
                            &group.name,
                            &mailbox.public_key,
                        )
                        .await
                    } else {
                        self.send_invite_impl(invitee, group.id, &group.name, &mailbox.public_key)
                            .await
                    }
                } else {
                    self.send_invite_impl(invitee, group.id, &group.name, &mailbox.public_key)
                        .await
                };

            if let Err(e) = send_result {
                tracing::error!("Failed to send invite to {}: {}", invitee, e);
            }
        }

        tracing::info!(
            "Created anonymous group {} with {} invites (with roles)",
            group.id,
            invitee_count
        );

        Ok(group)
    }

    /// Create a mailbox for a group using blind signature tokens.
    pub(crate) async fn create_group_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<AnonGroupMailbox, AnonGroupError> {
        // Get or request a blind-signed token for anonymous mailbox registration
        let token = match self.token_manager.get_available_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                // Request new tokens from server
                let mut mailbox_client =
                    MailboxClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone());
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
        let public_key: [u8; 32] = token
            .public_key
            .clone()
            .try_into()
            .map_err(|_| AnonGroupError::InvalidData("Invalid public key length".to_string()))?;
        let private_key: [u8; 64] = token
            .private_key
            .clone()
            .try_into()
            .map_err(|_| AnonGroupError::InvalidData("Invalid private key length".to_string()))?;

        // Register the mailbox with the server using the blind signature
        let ttl_seconds = 30 * 24 * 60 * 60; // 30 days in seconds
        let mut mailbox_client =
            MailboxClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone());

        let response = mailbox_client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                ttl_seconds,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| {
                AnonGroupError::MessageError(format!("Failed to create mailbox on server: {}", e))
            })?;

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

    /// Accept an invite and join a group.
    ///
    /// Creates our mailbox, stores the group, and sends JoinAck to the inviter.
    pub async fn accept_invite(
        &self,
        group_id: AnonGroupId,
        group_name: String,
        owner_id: UserId,
        inviter_id: UserId,
        inviter_mailbox_pk: [u8; 32],
        inviter_profile_key: Option<[u8; 32]>,
        inviter_display_name: Option<String>,
        inviter_avatar: Option<Vec<u8>>,
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Create group locally as member
        let group = AnonGroup::from_invite(group_id, group_name, owner_id);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group_id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as member with our profile
        let profile_key = self.get_my_profile_key();
        let my_display_name = self.get_my_display_name();
        let my_avatar = self.get_my_avatar();
        let my_member = AnonGroupMember::with_mailbox(
            group_id,
            current_user,
            AnonGroupRole::Member,
            mailbox.public_key,
        )
        .with_profile(profile_key, my_display_name, my_avatar);
        self.anon_group_repo.add_member(&my_member)?;

        // 4. Add inviter with their mailbox key and profile
        let inviter_member = AnonGroupMember::with_mailbox(
            group_id,
            inviter_id,
            AnonGroupRole::Owner, // Assume owner until we get member list
            inviter_mailbox_pk,
        )
        .with_profile(inviter_profile_key, inviter_display_name.clone(), inviter_avatar);
        self.anon_group_repo.add_member(&inviter_member)?;

        tracing::debug!(
            "Stored inviter {} with display_name={:?} in group {}",
            inviter_id,
            inviter_display_name,
            group_id
        );

        // 5. Send JoinAck to inviter's anonymous mailbox
        self.send_join_ack(&inviter_id, group_id, &inviter_mailbox_pk, &mailbox.public_key)
            .await?;

        tracing::info!(
            "Joined anonymous group {} from inviter {}",
            group_id,
            inviter_id
        );
        Ok(group)
    }
}
