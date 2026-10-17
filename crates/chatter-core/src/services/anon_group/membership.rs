//! Member management and roster operations.

use std::time::Duration;

use prost::Message as ProstMessage;
use rand::RngCore;
use tokio::time::sleep;

use crate::domain::UserId;
use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMember, AnonGroupRole, PendingActionType,
};
use crate::proto::content_v1::{
    self, AnonDmMessage as AnonDmMessageProto, AnonGroupInvite, AnonGroupJoinAck,
    AnonGroupMemberInfo, AnonGroupMemberList, AnonGroupProfileShare, Content,
};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

#[cfg(test)]
tokio::task_local! {
    static RECIPIENT_PREPARED: (std::sync::Arc<tokio::sync::Notify>, std::sync::Arc<tokio::sync::Notify>);
}

impl AnonGroupService {
    /// Send content to a member's anonymous group mailbox.
    ///
    /// Uses pairwise Signal Protocol encryption (forward secrecy) and delivers
    /// directly to the member's anonymous mailbox. Does NOT use DM infrastructure.
    ///
    /// # Arguments
    /// * `peer_id` - The recipient's user ID (for Signal Protocol session)
    /// * `peer_device` - The recipient device that owns `mailbox_pk`
    ///   ([`AnonGroupMember::device_id`]); `None` refuses — the only device that
    ///   could open the message is unknown, and no other device is guessed.
    /// * `mailbox_pk` - The recipient's anonymous group mailbox public key
    /// * `content` - The Content proto to send
    pub(crate) async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        peer_device: Option<u32>,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonGroupError> {
        self.check_group_recipient(&content, peer_id, mailbox_pk)?;
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

        // No known recipient device: nothing could open the message, and no
        // other device of theirs is guessed. Placed after the submission
        // lookup so an already-claimed post still reports its own state.
        let Some(device) = peer_device else {
            if let Some(key) = retained.as_ref() {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            return Err(AnonGroupError::InvalidData(
                "recipient is not bound to a device".into(),
            ));
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
        let has_session = self
            .pairwise_service
            .has_session(peer_id, device)
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
        // Secret-chat traffic goes over Tor or not at all; refuse before any
        // network contact, token spend or encryption. Placed after the
        // submission lookup so an already-claimed post still reports its own
        // (local) state, and a fresh claim is released here.
        if let Err(e) = self.grpc_pool.secret_channel() {
            if let Some(key) = retained.as_ref() {
                crate::services::gateway::submission::discard_before_encryption(
                    self.settings_repo.database(),
                    key,
                );
            }
            return Err(AnonGroupError::MessageError(e.to_string()));
        }
        // Our own sealed-sender certificate first (fetched over Tor if it
        // must be — never the normal transport), so a missing one fails before
        // any token is spent. Names no recipient.
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
                AnonGroupError::from(e)
            })?;
        let ciphertext = if has_session {
            self.check_group_recipient(&content, peer_id, mailbox_pk)?;
            self.admit_recipient_encryption(retained.as_deref())?;
            // Local only; a session that vanished since the check fails here
            // rather than being re-established.
            self.pairwise_service
                .encrypt_existing_session(peer_id, device, &plaintext)
                .await?
        } else {
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
            self.check_group_recipient(&content, peer_id, mailbox_pk)?;
            self.admit_recipient_encryption(retained.as_deref())?;
            self.pairwise_service
                .encrypt_anonymous(peer_id, Some(device), &plaintext, &token)
                .await?
                .0
        };

        self.check_group_recipient(&content, peer_id, mailbox_pk)?;
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
            .send_to_secret_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
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

    /// Revalidate snapshots at encryption and delivery boundaries, including
    /// queue drains. Removal controls deliberately still reach the removed peer.
    fn check_group_recipient(
        &self,
        content: &Content,
        peer: &UserId,
        key: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        use content_v1::content::Content as Kind;
        let scope = match content.content.as_ref() {
            Some(Kind::AnonGroupMessage(message)) => Some((&message.group_id, content.timestamp)),
            Some(Kind::AnonGroupJoinAck(ack)) => Some((&ack.group_id, ack.timestamp)),
            Some(Kind::AnonGroupMemberList(list)) => Some((&list.group_id, list.timestamp)),
            Some(Kind::AnonGroupProfileShare(profile)) => {
                Some((&profile.group_id, profile.timestamp))
            }
            _ => None,
        };
        if let Some((id, timestamp)) = scope {
            let group = AnonGroupId::try_from_slice(id)
                .ok_or_else(|| AnonGroupError::InvalidData("Invalid recipient group".into()))?;
            if !self
                .anon_group_repo
                .current_recipient(&group, peer, key, timestamp)?
            {
                return Err(AnonGroupError::MemberNotFound(*peer));
            }
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

    /// V1 invitations bind the authenticated sender as the invitee's owner.
    /// Admin delegation needs an owner-authorized protocol before it is safe.
    fn require_invitation_owner(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;
        if group.is_blocked() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }
        let member = self.anon_group_repo.get_member(group_id, &current_user)?;
        if group.owner_id != current_user
            || group.my_role != AnonGroupRole::Owner
            || !member.is_some_and(|m| m.role == AnonGroupRole::Owner)
        {
            return Err(AnonGroupError::InsufficientPermissions(
                "Only the group owner can send invitations; admin invitations require owner authorization".to_string(),
            ));
        }
        Ok(())
    }

    /// Send a group invite over the secret channel, never the friend DM:
    /// through a ready anon DM tunnel with the invitee when one exists, else
    /// anonymously to their device inbox. Either way over Tor, so the server
    /// never learns who is inviting whom into a secret group.
    pub(crate) async fn send_invite(
        &self,
        invitee: UserId,
        group_id: AnonGroupId,
        group_name: &str,
        inviter_mailbox_pk: &[u8; 32],
    ) -> Result<(), AnonGroupError> {
        self.require_invitation_owner(&group_id)?;
        let profile_key = self.get_my_profile_key();
        let mut encrypted_display_name = None;
        let mut avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            avatar = self.my_avatar_pointer().await;
        }

        let invite = Content {
            content: Some(content_v1::content::Content::AnonGroupInvite(
                AnonGroupInvite {
                    group_id: group_id.to_vec(),
                    group_name: group_name.to_string(),
                    inviter_mailbox_pk: inviter_mailbox_pk.to_vec(),
                    timestamp: TimestampNanos::now().as_u64(),
                    profile_key: profile_key.map(|k| k.to_vec()),
                    encrypted_display_name,
                    avatar,
                },
            )),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Vec::new(),
        };

        let tunnel = self
            .anon_dm_repo
            .get_live_conversation_by_peer(&invitee)?
            .filter(|dm| dm.is_ready())
            .and_then(|dm| dm.peer_mailbox_pk.map(|pk| (dm.id, dm.peer_device_id, pk)));
        if let Some((conversation_id, peer_device, peer_mailbox_pk)) = tunnel {
            let outer = Content {
                content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
                    AnonDmMessageProto {
                        conversation_id: conversation_id.to_vec(),
                        inner_content: Some(Box::new(invite)),
                    },
                ))),
                ephemeral_reply_to: None,
                timestamp: TimestampNanos::now().as_u64(),
                padding: Self::generate_padding(160),
            };
            // The secret DM's own device binding: only that device holds the
            // tunnel, so only it receives (and can accept) the invite.
            self.send_to_anon_mailbox(&invitee, peer_device, &peer_mailbox_pk, outer)
                .await?;
            tracing::debug!("Sent group invite via anon DM tunnel");
        } else {
            let invite = Content {
                padding: Self::generate_padding(160),
                ..invite
            };
            // Sealed to ONE device of the invitee (the server's current one):
            // that device alone can open the invite, so it is the one that
            // accepts, and its JoinAck binds the member to it.
            self.pairwise_service
                .send_to_device_inbox(
                    &self.token_manager,
                    &uuid::Uuid::new_v4().to_string(),
                    &invitee,
                    None,
                    &invite.encode_to_vec(),
                    |_| Ok(()),
                )
                .await?;
            tracing::debug!("Sent group invite anonymously");
        }
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
        let current = self.current_user_id()?;
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;
        let now = TimestampNanos::now().as_u64();
        if group.is_blocked()
            || group.owner_id != *inviter_id
            || !self
                .anon_group_repo
                .current_recipient(&group_id, &current, my_mailbox_pk, now)?
            || !self.anon_group_repo.current_recipient(
                &group_id,
                inviter_id,
                inviter_mailbox_pk,
                now,
            )?
        {
            return Err(AnonGroupError::InvalidData(
                "JoinAck requires current self and owner membership".into(),
            ));
        }
        let profile_key = self.get_my_profile_key();
        let mut encrypted_display_name = None;
        let mut avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            avatar = self.my_avatar_pointer().await;
        }

        let ack = AnonGroupJoinAck {
            group_id: group_id.to_vec(),
            joiner_mailbox_pk: my_mailbox_pk.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
            profile_key: profile_key.map(|k| k.to_vec()),
            encrypted_display_name,
            avatar,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupJoinAck(ack)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

        // Send to inviter's anonymous group mailbox (not DM), on the device
        // the invite came from.
        let inviter_device = self
            .anon_group_repo
            .get_member(&group_id, inviter_id)?
            .and_then(|m| m.device_id);
        self.send_to_anon_mailbox(inviter_id, inviter_device, inviter_mailbox_pk, content)
            .await?;

        tracing::debug!("Sent JoinAck for group to");
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
        let mut avatar = None;

        if let Some(pk) = profile_key {
            if let Some(name) = self.get_my_display_name() {
                if !name.is_empty() {
                    encrypted_display_name =
                        crate::crypto::profile_crypto::encrypt_display_name(&pk, &name).ok();
                }
            }
            avatar = self.my_avatar_pointer().await;
        }

        let profile_share = AnonGroupProfileShare {
            group_id: group_id.to_vec(),
            profile_key: profile_key.map(|k| k.to_vec()),
            encrypted_display_name,
            avatar,
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

        let device = self
            .anon_group_repo
            .get_member(group_id, member_id)?
            .and_then(|m| m.device_id);
        self.send_to_anon_mailbox(member_id, device, member_mailbox_pk, content)
            .await?;

        tracing::debug!("Sent profile share for group to");
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
                tracing::warn!("Failed to send profile share to in group: {}", e);
            } else {
                share_count += 1;
            }
        }

        tracing::info!("Shared profile with {} members in group", share_count);
        Ok(())
    }

    /// Handle a JoinAck from an invitee: record its mailbox key, device and
    /// profile, and broadcast the updated member list.
    pub async fn handle_join_ack_ordered(
        &self,
        group_id: AnonGroupId,
        joiner_id: UserId,
        // The sealed-sender-authenticated device the JoinAck came from: it
        // owns `joiner_mailbox_pk`, so the member is bound to it.
        joiner_device_id: u32,
        joiner_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
        timestamp: u64,
    ) -> Result<bool, AnonGroupError> {
        // 1. Verify the group exists and we're the owner/admin
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        if !group.is_admin_or_owner() {
            // SECURITY: If we're not owner/admin, we don't handle JoinAcks.
            // Regular members learn about new members via MemberList broadcasts from the owner.
            tracing::warn!("Rejected JoinAck for a secret group - not owner/admin");
            return Ok(false);
        }

        let member = AnonGroupMember {
            device_id: Some(joiner_device_id),
            ..AnonGroupMember::with_mailbox(
                group_id,
                joiner_id,
                AnonGroupRole::Member,
                joiner_mailbox_pk,
            )
            .with_profile(profile_key, display_name, avatar)
        };
        if !self
            .anon_group_repo
            .apply_ordered_join_ack(&member, timestamp)?
        {
            return Ok(false);
        }

        // 4. Drain any messages queued for this member while mailbox_pk was
        //    unknown. Failures are logged and left for retry (rows persist).
        if let Err(e) = self
            .drain_pending_messages(&group_id, &joiner_id, &joiner_mailbox_pk)
            .await
        {
            tracing::warn!(
                error = %e,
                "Failed to drain pending anon group messages after JoinAck"
            );
        }

        self.store_system_message(
            &group_id,
            Some(joiner_id),
            crate::domain::message::SystemMessage::UserJoined(joiner_id),
            timestamp,
        )?;

        // 5. Broadcast updated member list to all confirmed members
        if group.is_owner() {
            self.broadcast_member_list(&group_id).await?;
        }

        tracing::info!("Member joined group with mailbox key");
        Ok(true)
    }

    /// Broadcast the current member list to all confirmed members.
    pub(crate) async fn broadcast_member_list(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;
        let (snapshot, roster_timestamp) = self.anon_group_repo.ordered_roster_snapshot(
            group_id,
            &current_user,
            TimestampNanos::now().as_u64(),
        )?;
        let admissions: std::collections::HashMap<UserId, u64> =
            snapshot.iter().map(|(m, ts)| (m.user_id, *ts)).collect();
        let members: Vec<_> = snapshot.into_iter().map(|(m, _)| m).collect();
        // Our own entry carries this device: it owns our group mailbox.
        let my_device: Option<u32> = self
            .settings_repo
            .get(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)?;
        let confirmed_members: Vec<_> = members
            .iter()
            .filter(|m| m.mailbox_pk.is_some())
            .cloned()
            .collect();

        // Build member info list. Each member's avatar is relayed as a
        // pointer: uploaded by us once per distinct avatar, then cached.
        let mut member_infos: Vec<AnonGroupMemberInfo> = Vec::with_capacity(members.len());
        for m in &members {
            let profile_key = m.profile_key.map(|k| k.to_vec());
            let mut encrypted_display_name = None;
            let mut avatar = None;

            if let Some(pk) = m.profile_key {
                if let Some(ref name) = m.display_name {
                    if !name.is_empty() {
                        match crate::crypto::profile_crypto::encrypt_display_name(&pk, name) {
                            Ok(enc) => encrypted_display_name = Some(enc),
                            Err(e) => {
                                tracing::warn!("Failed to encrypt display name for: {}", e)
                            }
                        }
                    }
                }
                if let Some(ref av) = m.avatar {
                    avatar = self.avatar_pointer(av).await;
                }
            } else {
                tracing::debug!(
                    "No profile key for member in group - cannot encrypt profile for broadcast"
                );
            }

            member_infos.push(AnonGroupMemberInfo {
                user_id: m.user_id.as_bytes().to_vec(),
                mailbox_pk: m.mailbox_pk.map(|pk| pk.to_vec()).unwrap_or_default(),
                role: m.role.to_i32(),
                profile_key,
                encrypted_display_name,
                avatar,
                admission_timestamp: admissions[&m.user_id],
                device_id: if m.user_id == current_user {
                    my_device
                } else {
                    m.device_id
                }
                .unwrap_or(0),
            });
        }

        let member_list = AnonGroupMemberList {
            group_id: group_id.to_vec(),
            members: member_infos,
            timestamp: roster_timestamp,
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
                .send_to_anon_mailbox(
                    &member.user_id,
                    member.device_id,
                    &mailbox_pk,
                    content.clone(),
                )
                .await
            {
                tracing::error!("Failed to send member list to: {}", e);
            }
        }

        tracing::debug!(
            "Broadcasting member list for group to {} members",
            member_count.saturating_sub(1)
        );
        Ok(())
    }

    /// Fetch the avatars a member list points at (each fetch retried after
    /// each of `delays`) and store each that differs from the one we hold.
    /// Our own entry is skipped. Returns whether any member's avatar changed,
    /// and the entries whose every fetch failed, for a later pass.
    pub async fn refresh_roster_avatars(
        &self,
        group_id: &AnonGroupId,
        members: &[AnonGroupMemberInfo],
        delays: &[std::time::Duration],
    ) -> Result<(bool, Vec<AnonGroupMemberInfo>), AnonGroupError> {
        let current_user = self.current_user_id()?;
        let mut changed = false;
        let mut failed = Vec::new();
        for info in members {
            let (Some(pointer), Some(user_id)) =
                (info.avatar.as_ref(), UserId::try_from_slice(&info.user_id))
            else {
                continue;
            };
            if user_id == current_user {
                continue;
            }
            let Some(member) = self.anon_group_repo.get_member(group_id, &user_id)? else {
                continue;
            };
            let held = member.avatar.clone();
            let avatar = match self
                .fetch_member_avatar(pointer, held.clone(), delays)
                .await
            {
                Ok(Some(avatar)) => avatar,
                Ok(None) => continue,
                Err(_) => {
                    failed.push(info.clone());
                    continue;
                }
            };
            if held.as_ref() != Some(&avatar) {
                self.anon_group_repo.update_member_profile(
                    group_id,
                    &user_id,
                    None,
                    None,
                    Some(avatar),
                )?;
                changed = true;
            }
        }
        Ok((changed, failed))
    }

    /// Handle an incoming member list update.
    pub fn handle_member_list(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        members: Vec<AnonGroupMemberInfo>,
    ) -> Result<(), AnonGroupError> {
        self.handle_member_list_ordered(group_id, sender_id, members, 0)
            .map(|_| ())
    }

    /// All transports must pass the timestamp inside the authenticated payload.
    pub fn handle_member_list_ordered(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        members: Vec<AnonGroupMemberInfo>,
        timestamp: u64,
    ) -> Result<bool, AnonGroupError> {
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
            tracing::warn!("Rejected member list from non-owner");
            return Err(AnonGroupError::InsufficientPermissions(
                "Only the group owner can send member lists".to_string(),
            ));
        }

        let mut parsed = Vec::with_capacity(members.len());
        let mut seen = std::collections::HashSet::new();
        // Parse the complete snapshot before any writes.
        for member_info in members {
            let user_id = UserId::try_from_slice(&member_info.user_id)
                .ok_or_else(|| AnonGroupError::InvalidData("Invalid user ID".to_string()))?;

            if !seen.insert(user_id) {
                return Err(AnonGroupError::InvalidData(
                    "Duplicate roster member".into(),
                ));
            }

            // Preserve our locally owned mailbox/profile, but persist our
            // owner-authenticated admission too. Otherwise a delayed old kick
            // could evict us again after we explicitly accepted a reinvitation.
            if user_id == current_user {
                if let Some(local) = self.anon_group_repo.get_member(&group_id, &user_id)? {
                    parsed.push((local, member_info.admission_timestamp));
                }
                continue;
            }

            // Role in 0..=2 and a mailbox key that is empty or 32 bytes are
            // content.proto rules, enforced where the member list was decoded.
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

            use crate::crypto::profile_crypto::decrypt_display_name;

            let display_name: Option<String> =
                match (&profile_key, &member_info.encrypted_display_name) {
                    (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                        match decrypt_display_name(pk, encrypted) {
                            Ok(name) if !name.is_empty() => {
                                tracing::debug!("Decrypted member display name");
                                Some(name)
                            }
                            Ok(_) => {
                                tracing::debug!("Decrypted empty display name for");
                                None
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decrypt display name for: {}", e);
                                None
                            }
                        }
                    }
                    _ => {
                        tracing::trace!("No profile key or encrypted name for member in list");
                        None
                    }
                };

            // Avatars are pointers fetched after the roster is applied
            // (`refresh_roster_avatars`); `None` keeps what we hold.
            let avatar: Option<Vec<u8>> = None;

            let member = if let Some(pk) = mailbox_pk {
                AnonGroupMember::with_mailbox(group_id, user_id, role, pk)
            } else {
                AnonGroupMember::new(group_id, user_id, role)
            }
            .with_profile(profile_key, display_name, avatar);
            let member = AnonGroupMember {
                device_id: (member_info.device_id != 0).then_some(member_info.device_id),
                ..member
            };
            parsed.push((member, member_info.admission_timestamp));
        }

        let had_roster = self.anon_group_repo.has_applied_roster(&group_id)?;
        let confirmed_before: std::collections::HashSet<UserId> = self
            .anon_group_repo
            .get_members(&group_id)?
            .into_iter()
            .filter(|member| member.mailbox_pk.is_some())
            .map(|member| member.user_id)
            .collect();
        let inactive_before: std::collections::HashSet<UserId> = self
            .anon_group_repo
            .inactive_member_ids(&group_id)?
            .into_iter()
            .collect();
        let applied = self
            .anon_group_repo
            .apply_ordered_roster(&group_id, &sender_id, &current_user, timestamp, &parsed)?;
        if applied {
            for member in self.anon_group_repo.get_members(&group_id)? {
                if member.mailbox_pk.is_some()
                    && !confirmed_before.contains(&member.user_id)
                    && (had_roster || inactive_before.contains(&member.user_id))
                {
                    // The roster timestamp is the stable notice version. A
                    // replay is ignored by the ordered roster and message ID.
                    self.store_system_message(
                        &group_id,
                        Some(member.user_id),
                        crate::domain::message::SystemMessage::UserJoined(member.user_id),
                        timestamp,
                    )?;
                }
            }
        }
        Ok(applied)
    }

    /// Owner-only eligibility for retrying the existing pending admission.
    pub fn can_retry_pending_invitation(
        &self,
        group_id: &AnonGroupId,
        invitee: &UserId,
    ) -> Result<bool, AnonGroupError> {
        self.require_invitation_owner(group_id)?;
        Ok(self
            .anon_group_repo
            .can_retry_invitation(group_id, invitee)?)
    }

    /// Invite additional members to an existing group with specified roles.
    ///
    /// Only the real owner can invite: v1 has no owner-authorized admin delegation.
    /// Invites are sent via the same
    /// mechanism as during group creation (DM or anon DM tunnel).
    pub async fn invite_members(
        &self,
        group_id: &AnonGroupId,
        invitees: Vec<(UserId, AnonGroupRole)>,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists and we have permission to invite
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        self.require_invitation_owner(group_id)?;

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
        let mut first_send_error = None;
        for (invitee, role) in invitees {
            // An explicit retry reuses the pending admission and its original role.
            // Confirmed, expired and removed memberships are never reopened here.
            let retry_pending = self
                .anon_group_repo
                .can_retry_invitation(group_id, &invitee)?;
            if existing_members.contains(&invitee) && !retry_pending {
                tracing::debug!("User is already a member of group");
                continue;
            }

            // Add as pending member with specified role
            let member = AnonGroupMember::new(*group_id, invitee, role);
            if !retry_pending
                && !self.anon_group_repo.invite_ordered_member(
                    &member,
                    &current_user,
                    TimestampNanos::now().as_u64(),
                )?
            {
                self.require_invitation_owner(group_id)?;
                if self
                    .anon_group_repo
                    .get_member(group_id, &invitee)?
                    .is_some()
                {
                    continue;
                }
                return Err(AnonGroupError::InvalidData(
                    "Invitation admission was refused".to_string(),
                ));
            }

            // Send invite with small random delay to reduce traffic correlation
            let delay_ms = 100 + (rand::rng().next_u32() % 400);
            sleep(Duration::from_millis(delay_ms as u64)).await;

            let send_result = self
                .send_invite(invitee, *group_id, &group.name, &mailbox.public_key)
                .await;

            match send_result {
                Ok(_) => invited_count += 1,
                Err(e) => {
                    tracing::error!("Failed to send invite to: {}", e);
                    if first_send_error.is_none() {
                        first_send_error = Some(e);
                    }
                }
            }
        }

        tracing::info!("Invited {} members to group", invited_count);

        if let Some(error) = first_send_error {
            return Err(error);
        }
        Ok(())
    }

    /// Get members of a group.
    pub fn get_members(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<AnonGroupMember>, AnonGroupError> {
        Ok(self.anon_group_repo.get_members(group_id)?)
    }

    /// Count joined members and pending invitees from the local roster.
    pub fn member_counts(&self, group_id: &AnonGroupId) -> Result<(usize, usize), AnonGroupError> {
        let members = self.get_members(group_id)?;
        let joined = members
            .iter()
            .filter(|member| member.mailbox_pk.is_some())
            .count();
        Ok((joined, members.len() - joined))
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

    /// Retain the last shared avatar for departed members' message history.
    pub fn get_member_avatar(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<Vec<u8>>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member_avatar(group_id, user_id)?)
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
        let member = self
            .anon_group_repo
            .get_member(&group_id, &member_id)?
            .ok_or(AnonGroupError::MemberNotFound(member_id))?;
        if profile_key.is_none_or(|value| member.profile_key == Some(value))
            && display_name
                .as_ref()
                .is_none_or(|value| member.display_name.as_ref() == Some(value))
            && avatar
                .as_ref()
                .is_none_or(|value| member.avatar.as_ref() == Some(value))
        {
            return Ok(());
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
                                "Failed to send profile update to member in group: {}",
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
            keyring::set_default_store(crate::system::keyring::test_credential_store());
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
        repo.add_member(&AnonGroupMember {
            device_id: Some(2),
            ..AnonGroupMember::with_mailbox(group_id, owner, AnonGroupRole::Owner, [1u8; 32])
        })
        .unwrap();
        repo.add_member(&AnonGroupMember {
            device_id: Some(2),
            ..AnonGroupMember::with_mailbox(group_id, mallory, AnonGroupRole::Member, [2u8; 32])
        })
        .unwrap();
        repo.add_member(&AnonGroupMember {
            device_id: Some(2),
            ..AnonGroupMember::with_mailbox(group_id, victim, AnonGroupRole::Member, VICTIM_PK)
        })
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
            avatar: None,
            admission_timestamp: 0,
            device_id: 0,
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
                    svc.send_to_anon_mailbox(&f.owner, Some(1), &[1; 32], content),
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

    /// With no Tor route an invite fails closed; it never falls back to the
    /// friend DM, which would show the server who is invited where.
    #[tokio::test]
    async fn invite_without_tor_fails_closed() {
        let f = fixture();
        let svc = f._client.anon_groups();
        let me = svc.current_user_id().unwrap();
        let group = AnonGroup::new_as_owner("mine".into(), me);
        let repo = &svc.anon_group_repo;
        repo.insert_group(&group).unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group.id,
            me,
            AnonGroupRole::Owner,
            [1u8; 32],
        ))
        .unwrap();

        let res = svc
            .send_invite(f.victim, group.id, "mine", &[1u8; 32])
            .await;
        assert!(
            matches!(&res, Err(AnonGroupError::MessageError(e)) if e.contains("Tor")),
            "{res:?}"
        );
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

        // Even the owner's list cannot crown a second Owner: refused whole.
        svc.handle_member_list(f.group_id, f.owner, forged(&f))
            .unwrap();
        assert_eq!(
            repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
            Some(AnonGroupRole::Member)
        );

        let list = vec![
            info(f.owner, AnonGroupRole::Owner, [1u8; 32]),
            info(f.mallory, AnonGroupRole::Admin, [2u8; 32]),
            info(f.victim, AnonGroupRole::Member, [9u8; 32]),
        ];
        svc.handle_member_list_ordered(f.group_id, f.owner, list, 1)
            .unwrap();
        assert_eq!(
            repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
            Some(AnonGroupRole::Admin)
        );
        let victim = repo.get_member(&f.group_id, &f.victim).unwrap().unwrap();
        assert_eq!(victim.mailbox_pk, Some([9u8; 32]));
    }

    /// RC107: a member's avatar whose fetch fails (here: no route) is
    /// reported back for a retry pass, not dropped; an entry with no usable
    /// pointer is not.
    #[tokio::test]
    async fn failed_roster_avatar_fetch_is_kept_for_a_retry() {
        let f = fixture();
        let svc = f._client.anon_groups();
        let mut fetchable = info(f.victim, AnonGroupRole::Member, [9u8; 32]);
        fetchable.avatar = Some(crate::proto::content_v1::AttachmentPointer {
            cdn_key: "k".into(),
            plaintext_size: 3,
            plaintext_hash: "00".into(),
            ..Default::default()
        });
        let mut unusable = info(f.mallory, AnonGroupRole::Member, [2u8; 32]);
        unusable.avatar = Some(Default::default());
        let (changed, failed) = svc
            .refresh_roster_avatars(
                &f.group_id,
                &[fetchable.clone(), unusable],
                &[std::time::Duration::ZERO],
            )
            .await
            .unwrap();
        assert!(!changed);
        assert_eq!(failed, vec![fetchable]);
    }
}
