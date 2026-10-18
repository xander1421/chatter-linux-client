use super::*;

impl AnonGroupService {
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

        let member_count = confirmed_members.len();
        let recipients: Vec<UserId> = confirmed_members
            .iter()
            .filter(|member| member.user_id != current_user)
            .map(|member| member.user_id)
            .collect();
        self.anon_group_repo.replace_pending_roster(
            group_id,
            &recipients,
            &content.encode_to_vec(),
        )?;
        // Each persisted row is removed only after a confirmed mailbox deposit.
        for member in &confirmed_members {
            if member.user_id == current_user {
                continue; // Don't send to ourselves
            }
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");
            let delay_ms = rand::rng().next_u32() % 5_000; // 0-5 second delay
            sleep(Duration::from_millis(delay_ms as u64)).await;
            if let Err(e) = self
                .drain_pending_messages(group_id, &member.user_id, &mailbox_pk)
                .await
            {
                tracing::error!("Failed to drain member list to {}: {}", member.user_id, e);
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
                    if local
                        .mailbox_pk
                        .as_ref()
                        .is_some_and(|key| member_info.mailbox_pk.as_slice() == key.as_slice())
                    {
                        parsed.push((local, member_info.admission_timestamp));
                    }
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
        let applied = self.anon_group_repo.apply_ordered_roster(
            &group_id,
            &sender_id,
            &current_user,
            timestamp,
            &parsed,
        )?;
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
}
