use super::*;

impl AnonGroupService {
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
        let my_user_id = self.settings_repo.current_user_id()?;
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
}
