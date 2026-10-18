use super::*;

impl AnonGroupService {
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

        // Existing members need the owner's pending-invite admission before
        // they post, so each poster can queue its own copy for the invitee.
        if invited_count > 0 {
            tracing::info!(group_id = %group_id, invited_count,
                "Broadcasting pending invitees to group posters");
            self.broadcast_member_list(group_id).await?;
        }

        if let Some(error) = first_send_error {
            return Err(error);
        }
        Ok(())
    }
}
