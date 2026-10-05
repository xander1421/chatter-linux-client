//! Member management and roster operations.

use std::time::Duration;

use prost::Message as ProstMessage;
use rand::RngCore;
use tokio::time::sleep;

use crate::domain::UserId;
use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMember, AnonGroupPending, AnonGroupRole, PendingActionType,
};
use crate::proto::content_v1::{
    self, AnonDmMessage as AnonDmMessageProto, AnonGroupInvite, AnonGroupJoinAck,
    AnonGroupMemberInfo, AnonGroupMemberList, AnonGroupProfileShare, Content,
};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::current_user::CurrentUserProvider;
use crate::services::message::SendDmContentParams;
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

#[cfg(test)]
tokio::task_local! {
    static RECIPIENT_PREPARED: (std::sync::Arc<tokio::sync::Notify>, std::sync::Arc<tokio::sync::Notify>);
}

impl AnonGroupService {
    /// Send content via existing DM channel.
    ///
    /// Used for **invites only** since the recipient doesn't have a group mailbox yet.
    /// This goes through DM infrastructure (bootstrap, two-layer encryption, DM mailbox).
    pub(crate) async fn send_via_dm(
        &self,
        peer_id: UserId,
        content: Content,
    ) -> Result<(), AnonGroupError> {
        let mut auth_client = self.grpc_pool.auth_client();

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
    pub(crate) async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonGroupError> {
        // 1. Serialize content
        let plaintext = content.encode_to_vec();
        let retained = if crate::services::gateway::submission::user_post(&content) {
            let owner = self.current_user_id()?;
            match crate::services::gateway::submission::prepare(
                self.settings_repo.database(),
                &owner.to_string(),
                &peer_id.to_string(),
                &plaintext,
            )? {
                crate::services::gateway::submission::Admission::First(key) => Some(key),
                crate::services::gateway::submission::Admission::Confirmed => {
                    crate::services::gateway::submission::mark_existing_submission();
                    return Ok(());
                }
                crate::services::gateway::submission::Admission::Unconfirmed => {
                    crate::services::gateway::submission::mark_existing_submission();
                    return Err(AnonGroupError::DeliveryUnconfirmed);
                }
            }
        } else {
            None
        };

        // 2. Encrypt with Signal Protocol (pairwise, no DM-specific layers).
        //
        // If a usable session already exists, encrypt locally — no prekey
        // fetch, no server contact, no token spent. On FIRST contact with a
        // member there is no session yet; establishing one via the normal
        // AUTHENTICATED prekey fetch would let the server observe "user X is
        // starting a session with member Y", and over a fan-out that
        // reconstructs the group roster this feature promises to hide. So we
        // bootstrap the session ANONYMOUSLY (blind token + anonymous prekey
        // fetch), matching the anon-DM design. Cost stays at ~1 token per new
        // peer (first contact only), not per message.
        //
        let has_session = self
            .pairwise_service
            .has_session(peer_id)
            .await
            .map_err(|e| {
                if let Some(key) = retained.as_ref() {
                    crate::services::gateway::submission::discard_before_encryption(
                        self.settings_repo.database(),
                        key,
                    );
                }
                AnonGroupError::CryptoError(e.to_string())
            })?;
        #[cfg(test)]
        if let Ok((ready, resume)) = RECIPIENT_PREPARED.try_with(Clone::clone) {
            ready.notify_one();
            resume.notified().await;
        }
        self.check_recipient_session(retained.as_deref())?;
        let ciphertext = if has_session {
            self.admit_recipient_encryption(retained.as_deref())?;
            self.pairwise_service
                .encrypt(peer_id, &plaintext)
                .await
                .map_err(|e| AnonGroupError::CryptoError(e.to_string()))?
        } else {
            // First contact: pre-warm our own sealed-sender cert before the
            // anonymous encrypt, which uses the cached cert only. Closes the
            // post-login race where the fire-and-forget login prefetch hasn't
            // landed yet. Self-cert fetch — no recipient linkage leaked.
            self.pairwise_service
                .ensure_sender_certificate()
                .await
                .map_err(|e| {
                    if let Some(key) = retained.as_ref() {
                        crate::services::gateway::submission::discard_before_encryption(
                            self.settings_repo.database(),
                            key,
                        );
                    }
                    AnonGroupError::CryptoError(e.to_string())
                })?;
            // Refuse an expired originating session before consuming a token.
            self.check_recipient_session(retained.as_deref())?;
            let token = self.claim_token().map_err(|e| {
                if let Some(key) = retained.as_ref() {
                    crate::services::gateway::submission::discard_before_encryption(
                        self.settings_repo.database(),
                        key,
                    );
                }
                e
            })?;
            self.admit_recipient_encryption(retained.as_deref())?;
            self.pairwise_service
                .encrypt_anonymous(peer_id, &plaintext, &token)
                .await
                .map_err(|e| AnonGroupError::CryptoError(e.to_string()))?
        };

        // 3. Send directly to anonymous mailbox
        if let Some(key) = retained.as_ref() {
            crate::services::gateway::submission::ciphertext(
                self.settings_repo.database(),
                key,
                &ciphertext,
            )
            .map_err(|_| AnonGroupError::DeliveryUnconfirmed)?;
        }
        self.mailbox_manager
            .send_to_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
            .await
            .map_err(|e| {
                if retained.is_some() {
                    AnonGroupError::DeliveryUnconfirmed
                } else {
                    AnonGroupError::MessageError(e.to_string())
                }
            })?;
        if let Some(key) = retained.as_ref() {
            crate::services::gateway::submission::confirm(self.settings_repo.database(), key)
                .map_err(|_| AnonGroupError::DeliveryUnconfirmed)?;
        }

        Ok(())
    }

    // A new lower claim can be discarded only before encryption. An existing
    // unknown claim marks upper uncertainty before reaching these helpers.
    fn check_recipient_session(&self, retained: Option<&str>) -> Result<(), AnonGroupError> {
        crate::services::gateway::submission::check_send_session().map_err(|e| {
            if let Some(key) = retained {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            e.into()
        })
    }

    fn admit_recipient_encryption(&self, retained: Option<&str>) -> Result<(), AnonGroupError> {
        crate::services::gateway::submission::mark_submission_started().map_err(|e| {
            if let Some(key) = retained {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            e.into()
        })
    }

    /// Claim a blind-signature token from the pre-fetched batch.
    ///
    /// Mirrors `AnonDmService::claim_token`. Used to fetch a member's prekey
    /// bundle anonymously on first contact so the server never learns the
    /// group membership graph from session establishment. Never requests
    /// tokens (NDSS'21 §VI-F); an empty batch fails this member's send.
    pub(crate) fn claim_token(
        &self,
    ) -> Result<crate::mailbox::token_manager::MailboxToken, AnonGroupError> {
        self.token_manager
            .claim_prefetched_token()
            .map_err(|e| AnonGroupError::CryptoError(e.to_string()))
    }

    /// Send an invite message to a user via their existing DM session.
    ///
    /// Invites use DM infrastructure since the recipient doesn't have a
    /// group mailbox yet.
    pub(crate) async fn send_invite_impl(
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
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
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

        tracing::debug!(
            "Sent invite for group {} to {} via regular DM",
            group_id,
            invitee
        );
        Ok(())
    }

    /// Send a group invite via an existing anon DM conversation.
    ///
    /// This routes the invite through the anon DM tunnel instead of regular DM,
    /// preserving anonymity for users who connected via anonymous messaging.
    pub(crate) async fn send_invite_via_anon_dm(
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
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
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
            content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
                dm_message,
            ))),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        // Send to peer's anon DM mailbox
        self.send_to_anon_mailbox(&anon_dm.peer_id, &peer_mailbox_pk, outer_content)
            .await?;

        tracing::debug!(
            "Sent invite for group {} to {} via anon DM tunnel",
            group_id,
            anon_dm.peer_id
        );
        Ok(())
    }

    /// Send a JoinAck message to the inviter's anonymous group mailbox.
    ///
    /// Uses pairwise encryption (Signal Protocol) and delivers directly to the
    /// inviter's anonymous mailbox - NOT through DM infrastructure.
    pub(crate) async fn send_join_ack(
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
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
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
        self.send_to_anon_mailbox(inviter_id, inviter_mailbox_pk, content)
            .await?;

        tracing::debug!("Sent JoinAck for group {} to {}", group_id, inviter_id);
        Ok(())
    }

    /// Send our profile to a specific group member.
    ///
    /// Used to share our display name/avatar directly with group members.
    async fn send_profile_share(
        &self,
        group_id: &AnonGroupId,
        member_id: &UserId,
        member_mailbox_pk: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        let profile_key = self.get_my_profile_key();
        let mut encrypted_display_name = None;
        let mut encrypted_avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            if let Some(av) = self.get_my_avatar() {
                encrypted_avatar = crate::crypto::profile_crypto::encrypt_avatar(&pk, &av).ok();
            }
        }

        let profile_share = AnonGroupProfileShare {
            group_id: group_id.to_vec(),
            profile_key: profile_key.map(|k| k.to_vec()),
            encrypted_display_name,
            encrypted_avatar,
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupProfileShare(
                profile_share,
            )),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        self.send_to_anon_mailbox(member_id, member_mailbox_pk, content)
            .await?;

        tracing::debug!("Sent profile share for group {} to {}", group_id, member_id);
        Ok(())
    }

    /// Share our profile with all group members.
    ///
    /// Called after receiving a member list to introduce ourselves to other members
    /// we didn't know about before. This ensures all members learn our display name
    /// directly rather than relying on the group owner to relay it.
    pub async fn share_profile_with_group(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;
        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        let mut share_count = 0;
        for member in members {
            // Skip ourselves
            if member.user_id == current_user {
                continue;
            }

            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            // Small random delay to avoid timing correlation
            let delay_ms = rand::rng().next_u32() % 2_000;
            sleep(Duration::from_millis(delay_ms as u64)).await;

            if let Err(e) = self
                .send_profile_share(group_id, &member.user_id, &mailbox_pk)
                .await
            {
                tracing::warn!(
                    "Failed to send profile share to {} in group {}: {}",
                    member.user_id,
                    group_id,
                    e
                );
            } else {
                share_count += 1;
            }
        }

        tracing::info!(
            "Shared profile with {} members in group {}",
            share_count,
            group_id
        );
        Ok(())
    }

    /// Handle a JoinAck from an invitee.
    ///
    /// Updates the member's mailbox key, profile, and broadcasts updated member list.
    pub async fn handle_join_ack(
        &self,
        group_id: AnonGroupId,
        joiner_id: UserId,
        joiner_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), AnonGroupError> {
        // 1. Verify the group exists and we're the owner/admin
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        if !group.is_admin_or_owner() {
            // SECURITY: If we're not owner/admin, we don't handle JoinAcks.
            // Regular members learn about new members via MemberList broadcasts from the owner.
            tracing::warn!(
                "Rejected JoinAck from {} for group {} - not owner/admin",
                joiner_id,
                group_id
            );
            return Ok(());
        }

        // 2. Update member's mailbox key and profile
        self.anon_group_repo.update_member_mailbox_and_profile(
            &group_id,
            &joiner_id,
            &joiner_mailbox_pk,
            profile_key,
            display_name.clone(),
            avatar,
        )?;

        tracing::debug!(
            "Updated member {} in group {} with profile: {:?}",
            joiner_id,
            group_id,
            display_name
        );

        // 3. Remove pending invite
        self.anon_group_repo
            .delete_pending(&group_id, &joiner_id, PendingActionType::Invite)?;

        // 4. Drain any messages queued for this member while mailbox_pk was
        //    unknown. Failures are logged and left for retry (rows persist).
        if let Err(e) = self
            .drain_pending_messages(&group_id, &joiner_id, &joiner_mailbox_pk)
            .await
        {
            tracing::warn!(
                group_id = %group_id,
                joiner_id = %joiner_id,
                error = %e,
                "Failed to drain pending anon group messages after JoinAck"
            );
        }

        // 5. Broadcast updated member list to all confirmed members
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
    pub(crate) async fn broadcast_member_list(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<(), AnonGroupError> {
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
                            match crate::crypto::profile_crypto::encrypt_display_name(&pk, name) {
                                Ok(enc) => encrypted_display_name = Some(enc),
                                Err(e) => tracing::warn!("Failed to encrypt display name for {}: {}", m.user_id, e),
                            }
                        }
                    }
                    if let Some(ref av) = m.avatar {
                        if !av.is_empty() {
                            match crate::crypto::profile_crypto::encrypt_avatar(&pk, av) {
                                Ok(enc) => encrypted_avatar = Some(enc),
                                Err(e) => tracing::warn!("Failed to encrypt avatar for {}: {}", m.user_id, e),
                            }
                        }
                    }
                } else {
                    tracing::debug!("No profile key for member {} in group {} - cannot encrypt profile for broadcast", m.user_id, group_id);
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
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            let delay_ms = rand::rng().next_u32() % 5_000; // 0-5 second delay
            sleep(Duration::from_millis(delay_ms as u64)).await;

            if let Err(e) = self
                .send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone())
                .await
            {
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

    /// Handle an incoming member list update.
    pub fn handle_member_list(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        members: Vec<AnonGroupMemberInfo>,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // SECURITY: the member list is owner-authoritative. Every producer of
        // one is owner-gated (`broadcast_member_list` callers at
        // handle_join_ack, update_member_profile, and refresh_group_mailbox);
        // a member announces its own mailbox/profile by JoinAck to the owner,
        // never by a list. So a list from anyone else — whichever transport
        // (group mailbox, DM, anon DM) carried it — could only be a forged
        // role or a re-pointed mailbox_pk, and is rejected whole. Same rule as
        // AnonGroupRoleChange in `handle_update`.
        if sender_id != group.owner_id {
            tracing::warn!(
                sender_id = %sender_id,
                owner_id = %group.owner_id,
                group_id = %group_id,
                "Rejected member list from non-owner"
            );
            return Err(AnonGroupError::InsufficientPermissions(
                "Only the group owner can send member lists".to_string(),
            ));
        }

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

            use crate::crypto::profile_crypto::{decrypt_avatar, decrypt_display_name};

            let display_name: Option<String> =
                match (&profile_key, &member_info.encrypted_display_name) {
                    (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                        match decrypt_display_name(pk, encrypted) {
                            Ok(name) if !name.is_empty() => {
                                tracing::debug!("Decrypted display name for {}: {}", user_id, name);
                                Some(name)
                            }
                            Ok(_) => {
                                tracing::debug!("Decrypted empty display name for {}", user_id);
                                None
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to decrypt display name for {}: {}",
                                    user_id,
                                    e
                                );
                                None
                            }
                        }
                    }
                    _ => {
                        tracing::trace!(
                            "No profile key or encrypted name for member {} in list",
                            user_id
                        );
                        None
                    }
                };

            let avatar: Option<Vec<u8>> = match (&profile_key, &member_info.encrypted_avatar) {
                (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                    match decrypt_avatar(pk, encrypted) {
                        Ok(av) => Some(av),
                        Err(e) => {
                            tracing::warn!("Failed to decrypt avatar for {}: {}", user_id, e);
                            None
                        }
                    }
                }
                _ => None,
            };

            // Check if member already exists
            let is_member = self.anon_group_repo.is_member(&group_id, &user_id)?;

            if is_member {
                // Update existing member
                self.anon_group_repo
                    .update_member_role(&group_id, &user_id, role)?;
                if let Some(pk) = mailbox_pk {
                    self.anon_group_repo.update_member_mailbox_and_profile(
                        &group_id,
                        &user_id,
                        &pk,
                        profile_key,
                        display_name,
                        avatar,
                    )?;
                }
            } else {
                // Add new member
                let member = if let Some(pk) = mailbox_pk {
                    AnonGroupMember::with_mailbox(group_id, user_id, role, pk).with_profile(
                        profile_key,
                        display_name,
                        avatar,
                    )
                } else {
                    AnonGroupMember::new(group_id, user_id, role).with_profile(
                        profile_key,
                        display_name,
                        avatar,
                    )
                };
                self.anon_group_repo.add_member(&member)?;
            }
        }

        tracing::debug!("Updated member list for group {}", group_id);
        Ok(())
    }

    /// Invite additional members to an existing group with specified roles.
    ///
    /// Only admins/owners can invite new members. Invites are sent via the same
    /// mechanism as during group creation (DM or anon DM tunnel).
    pub async fn invite_members(
        &self,
        group_id: &AnonGroupId,
        invitees: Vec<(UserId, AnonGroupRole)>,
    ) -> Result<(), AnonGroupError> {
        let _current_user = self.current_user_id()?;

        // Verify group exists and we have permission to invite
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if !group.my_role.can_invite() {
            return Err(AnonGroupError::InsufficientPermissions(
                "Only admins can invite members".to_string(),
            ));
        }

        // Get our mailbox for this group
        let mailbox = self
            .anon_group_repo
            .get_mailbox(group_id)?
            .ok_or_else(|| AnonGroupError::InvalidData("No mailbox for group".to_string()))?;

        let existing_members: std::collections::HashSet<UserId> = self
            .anon_group_repo
            .get_members(group_id)?
            .into_iter()
            .map(|m| m.user_id)
            .collect();

        let mut invited_count = 0;
        for (invitee, role) in invitees {
            // Skip if already a member
            if existing_members.contains(&invitee) {
                tracing::debug!("User {} is already a member of group {}", invitee, group_id);
                continue;
            }

            // Add as pending member with specified role
            let member = AnonGroupMember::new(*group_id, invitee, role);
            self.anon_group_repo.add_member(&member)?;

            // Track pending invite
            let pending = AnonGroupPending::new_invite(*group_id, invitee);
            self.anon_group_repo.insert_pending(&pending)?;

            // Send invite with small random delay to reduce traffic correlation
            let delay_ms = 100 + (rand::rng().next_u32() % 400);
            sleep(Duration::from_millis(delay_ms as u64)).await;

            // Check if invitee has an active anon DM conversation with us
            let send_result =
                if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_conversation_by_peer(&invitee) {
                    if anon_dm.is_ready() {
                        // Send via anon DM tunnel
                        self.send_invite_via_anon_dm(
                            &anon_dm,
                            *group_id,
                            &group.name,
                            &mailbox.public_key,
                        )
                        .await
                    } else {
                        // Anon DM not ready, try regular DM
                        self.send_invite_impl(invitee, *group_id, &group.name, &mailbox.public_key)
                            .await
                    }
                } else {
                    // No anon DM, use regular DM (friend)
                    self.send_invite_impl(invitee, *group_id, &group.name, &mailbox.public_key)
                        .await
                };

            match send_result {
                Ok(_) => invited_count += 1,
                Err(e) => tracing::error!("Failed to send invite to {}: {}", invitee, e),
            }
        }

        tracing::info!("Invited {} members to group {}", invited_count, group_id);

        Ok(())
    }

    /// Get members of a group.
    pub fn get_members(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<AnonGroupMember>, AnonGroupError> {
        Ok(self.anon_group_repo.get_members(group_id)?)
    }

    /// Get a member of a group.
    pub fn get_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<AnonGroupMember>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member(group_id, user_id)?)
    }

    /// Get a member's display name from their group profile.
    pub fn get_member_display_name(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<String>, AnonGroupError> {
        Ok(self
            .anon_group_repo
            .get_member_display_name(group_id, user_id)?)
    }

    /// Check if a user is a member of a group.
    pub fn is_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.is_member(group_id, user_id)?)
    }

    /// Get a member's role in a group.
    pub fn get_member_role(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<AnonGroupRole>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member_role(group_id, user_id)?)
    }

    /// Update a member's profile and broadcast the new roster if we are the owner.
    pub async fn handle_profile_share(
        &self,
        group_id: AnonGroupId,
        member_id: UserId,
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), AnonGroupError> {
        // 1. Verify membership
        if !self.anon_group_repo.is_member(&group_id, &member_id)? {
            return Err(AnonGroupError::MemberNotFound(member_id));
        }

        // 2. Update local profile
        self.anon_group_repo.update_member_profile(
            &group_id,
            &member_id,
            profile_key,
            display_name,
            avatar,
        )?;

        // 3. If we are owner, broadcast the update to everyone
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        if group.is_owner() {
            self.broadcast_member_list(&group_id).await?;
        }

        Ok(())
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
        self.anon_group_repo.update_member_mailbox_and_profile(
            group_id,
            user_id,
            &mailbox_pk,
            profile_key,
            display_name,
            avatar,
        )?;
        // Also remove pending invite
        let _ = self
            .anon_group_repo
            .delete_pending(group_id, user_id, PendingActionType::Invite);
        Ok(())
    }

    /// Update a member's profile (sync, no encryption).
    ///
    /// Called by the handler when a ProfileShare is received.
    pub fn update_member_profile(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), AnonGroupError> {
        self.anon_group_repo.update_member_profile(
            group_id,
            user_id,
            profile_key,
            display_name,
            avatar,
        )?;
        Ok(())
    }

    /// Broadcast profile update to all groups where we are the owner.
    ///
    /// Called when user updates their profile to notify all owned groups.
    pub async fn broadcast_profile_to_owned_groups(&self) -> Result<(), AnonGroupError> {
        let my_user_id = CurrentUserProvider::current_user_id(&*self.settings_repo)?;
        let groups = self.anon_group_repo.list_active_groups()?;

        for group in groups {
            // Only broadcast to groups we own
            if group.owner_id != my_user_id {
                continue;
            }

            // Get group members
            let members = self.anon_group_repo.get_members(&group.id)?;

            // Send profile share to each member
            for member in &members {
                // Skip ourselves
                if member.user_id == my_user_id {
                    continue;
                }

                // Send profile share to this member
                if let Some(mailbox_pk) = member.mailbox_pk.as_ref() {
                    let pk_bytes: &[u8] = mailbox_pk;
                    if pk_bytes.len() == 32 {
                        let mut pk = [0u8; 32];
                        pk.copy_from_slice(pk_bytes);

                        if let Err(e) = self
                            .send_profile_share(&group.id, &member.user_id, &pk)
                            .await
                        {
                            tracing::warn!(
                                "Failed to send profile update to member {} in group {}: {}",
                                member.user_id,
                                group.id,
                                e
                            );
                        }
                    }
                }
            }

            tracing::info!(
                "Broadcasted profile update to {} members in owned group {}",
                members.len() - 1, // Minus ourselves
                group.id
            );
        }

        Ok(())
    }

    /// Get a group by ID.
    pub fn get_group(&self, group_id: &AnonGroupId) -> Result<Option<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.get_group(group_id)?)
    }

    /// List all groups.
    pub fn list_groups(&self) -> Result<Vec<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.list_groups()?)
    }

    /// List only active (non-blocked) groups.
    pub fn list_active_groups(&self) -> Result<Vec<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.list_active_groups()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::repositories::settings_repo::keys;

    /// We are a plain member of a group owned by `owner`, with a second member
    /// `mallory` who has a known mailbox.
    struct Fixture {
        _client: std::sync::Arc<crate::client::ChatterClient>,
        _dir: tempfile::TempDir,
        group_id: AnonGroupId,
        owner: UserId,
        mallory: UserId,
        victim: UserId,
    }

    const VICTIM_PK: [u8; 32] = [7u8; 32];

    fn fixture() -> Fixture {
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
        let me = UserId::new();
        let s = &client.messages().settings_repo;
        s.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();

        let (owner, mallory, victim) = (UserId::new(), UserId::new(), UserId::new());
        let group_id = AnonGroupId::new();
        let repo = &client.anon_groups().anon_group_repo;
        repo.insert_group(&AnonGroup::from_invite(group_id, "g".into(), owner))
            .unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group_id,
            owner,
            AnonGroupRole::Owner,
            [1u8; 32],
        ))
        .unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group_id,
            mallory,
            AnonGroupRole::Member,
            [2u8; 32],
        ))
        .unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group_id,
            victim,
            AnonGroupRole::Member,
            VICTIM_PK,
        ))
        .unwrap();
        Fixture {
            _client: client,
            _dir: dir,
            group_id,
            owner,
            mallory,
            victim,
        }
    }

    fn info(user: UserId, role: AnonGroupRole, pk: [u8; 32]) -> AnonGroupMemberInfo {
        AnonGroupMemberInfo {
            user_id: user.as_bytes().to_vec(),
            mailbox_pk: pk.to_vec(),
            role: role.to_i32(),
            profile_key: None,
            encrypted_display_name: None,
            encrypted_avatar: None,
        }
    }

    /// The forged list: mallory promotes herself to Owner and re-points the
    /// victim's mailbox at a key she holds.
    fn forged(f: &Fixture) -> Vec<AnonGroupMemberInfo> {
        vec![
            info(f.mallory, AnonGroupRole::Owner, [2u8; 32]),
            info(f.victim, AnonGroupRole::Member, [9u8; 32]),
        ]
    }

    #[tokio::test]
    async fn real_group_fanout_refuses_replaced_session_after_recipient_preparation() {
        use crate::services::gateway::submission::{SEND_SESSION, SendSession, track_submission};
        for earlier_submission in [false, true] {
            let f = fixture();
            let state = f._client.auth_state();
            let me = f._client.anon_groups().current_user_id().unwrap();
            state.set_logged_in(me, 1, "old".into());
            let captured = SendSession::capture(state);
            let ready = std::sync::Arc::new(tokio::sync::Notify::new());
            let resume = std::sync::Arc::new(tokio::sync::Notify::new());
            let client = f._client.clone();
            let group = f.group_id;
            let gates = (ready.clone(), resume.clone());
            let task = tokio::spawn(SEND_SESSION.scope(
                captured,
                RECIPIENT_PREPARED.scope(
                    gates,
                    track_submission(
                        async move {
                            if earlier_submission {
                                crate::services::gateway::submission::mark_existing_submission();
                            }
                            client
                                .anon_groups()
                                .send_message(
                                    &group,
                                    crate::services::build_text_content("old content", 41),
                                )
                                .await
                        },
                        None,
                    ),
                ),
            ));
            ready.notified().await;
            // Same-account login replaces the captured session, too.
            state.set_logged_in(me, 1, "replacement".into());
            resume.notify_one();
            let (result, crossed, _) = task.await.unwrap();
            assert!(matches!(result, Err(AnonGroupError::Storage(_))));
            assert_eq!(crossed, earlier_submission);
            let db = f._client.messages().database.clone();
            let conn = db.connection().unwrap();
            let lower_claims: i64 = conn
                .query_row(
                    "SELECT count(*) FROM settings WHERE key LIKE 'send.anonymous.v1:%'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                lower_claims, 0,
                "pre-encryption lower claim must be released"
            );
            // The expired send must not become durable parked work.
            assert!(
                f._client
                    .anon_groups()
                    .anon_group_repo
                    .get_pending_messages_for_member(&group, &f.owner)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn existing_unknown_recipient_claim_stays_uncertain_after_session_replacement() {
        use crate::services::gateway::submission::{self, SEND_SESSION, SendSession};
        let f = fixture();
        let svc = f._client.anon_groups();
        let me = svc.current_user_id().unwrap();
        let state = f._client.auth_state();
        state.set_logged_in(me, 1, "old".into());
        let session = SendSession::capture(state);
        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(
                content_v1::AnonGroupMessage {
                    group_id: f.group_id.to_vec(),
                    inner_content: Some(Box::new(crate::services::build_text_content(
                        "retained", 42,
                    ))),
                },
            ))),
            ephemeral_reply_to: None,
            timestamp: 42,
            padding: vec![],
        };
        let bytes = content.encode_to_vec();
        let db = svc.settings_repo.database();
        let key =
            match submission::prepare(db, &me.to_string(), &f.owner.to_string(), &bytes).unwrap() {
                submission::Admission::First(key) => key,
                _ => panic!("new claim"),
            };
        submission::ciphertext(db, &key, b"original ciphertext").unwrap();
        state.set_logged_in(me, 1, "replacement".into());
        let (result, crossed, _) = SEND_SESSION
            .scope(
                session,
                submission::track_submission(
                    svc.send_to_anon_mailbox(&f.owner, &[1; 32], content),
                    None,
                ),
            )
            .await;
        assert!(matches!(result, Err(AnonGroupError::DeliveryUnconfirmed)));
        assert!(crossed);
        assert!(matches!(
            submission::prepare(db, &me.to_string(), &f.owner.to_string(), &bytes).unwrap(),
            submission::Admission::Unconfirmed
        ));
    }

    #[tokio::test]
    async fn non_owner_member_list_changes_nothing() {
        let f = fixture();
        let svc = f._client.anon_groups();
        let repo = &svc.anon_group_repo;

        let res = svc.handle_member_list(f.group_id, f.mallory, forged(&f));
        assert!(
            matches!(res, Err(AnonGroupError::InsufficientPermissions(_))),
            "{res:?}"
        );

        assert_eq!(
            repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
            Some(AnonGroupRole::Member)
        );
        let victim = repo.get_member(&f.group_id, &f.victim).unwrap().unwrap();
        assert_eq!(
            victim.mailbox_pk,
            Some(VICTIM_PK),
            "a non-owner must not re-point another member's mailbox"
        );

        // Nor can a stranger who merely knows the group_id.
        let stranger = UserId::new();
        assert!(
            svc.handle_member_list(
                f.group_id,
                stranger,
                vec![info(stranger, AnonGroupRole::Admin, [3u8; 32])]
            )
            .is_err()
        );
        assert!(!repo.is_member(&f.group_id, &stranger).unwrap());
    }

    #[tokio::test]
    async fn owner_member_list_is_applied() {
        let f = fixture();
        let svc = f._client.anon_groups();
        let repo = &svc.anon_group_repo;

        svc.handle_member_list(f.group_id, f.owner, forged(&f))
            .unwrap();

        assert_eq!(
            repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
            Some(AnonGroupRole::Owner)
        );
        let victim = repo.get_member(&f.group_id, &f.victim).unwrap().unwrap();
        assert_eq!(victim.mailbox_pk, Some([9u8; 32]));
    }
}
