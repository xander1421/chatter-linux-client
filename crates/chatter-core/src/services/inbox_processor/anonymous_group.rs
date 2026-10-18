use super::*;

impl InboxEventProcessor {
    pub(super) async fn process_anonymous_group(&self, event: &InboxEvent) {
        match event {
            // =================================================================
            InboxEvent::AnonGroupInvite {
                peer_id,
                sender_device_id,
                group_id,
                group_name,
                inviter_mailbox_pk,
                timestamp: _,
                profile_key,
                encrypted_display_name,
                avatar,
            } => {
                self.handle_anon_group_invite(
                    *peer_id,
                    *sender_device_id,
                    group_id,
                    group_name,
                    inviter_mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    avatar,
                );
                // Return false - UI needs to add sidebar entry
            }

            InboxEvent::AnonGroupJoinAck {
                peer_id,
                sender_device_id,
                group_id,
                joiner_mailbox_pk,
                timestamp,
                profile_key,
                encrypted_display_name,
                avatar,
            } => {
                self.handle_anon_group_join_ack(
                    *peer_id,
                    *sender_device_id,
                    group_id,
                    joiner_mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    avatar,
                    *timestamp,
                );
            }

            InboxEvent::AnonGroupMemberList {
                peer_id,
                group_id,
                members,
                timestamp,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    group_id = ?group_id,
                    member_count = members.len(),
                    timestamp = timestamp,
                    "Received anonymous group member list"
                );

                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupMemberList");
                            return;
                        }
                    };

                let anon_service = self.anon_group_service.clone();
                let event_bus = self.event_bus.clone();
                let members = members.clone();
                let sender_id = *peer_id;
                let timestamp = *timestamp;
                tokio::spawn(async move {
                    let service = anon_service.clone();
                    let roster = members.clone();
                    let applied = tokio::task::spawn_blocking(move || {
                        service.handle_member_list_ordered(
                            anon_group_id,
                            sender_id,
                            roster,
                            timestamp,
                        )
                    })
                    .await;
                    match applied {
                        Ok(Ok(true)) => {
                            if let Err(error) = anon_service
                                .drain_pending_after_roster(&anon_group_id)
                                .await
                            {
                                tracing::warn!(%error, "Pending group post drain after roster failed");
                            }
                            event_bus.emit_domain(DomainEvent::AnonGroupMemberListReceived {
                                group_id: anon_group_id,
                                members: members.clone(),
                            });
                            if let Err(error) = anon_service
                                .retry_held_posts_for_group(&anon_group_id)
                                .await
                            {
                                tracing::warn!(%error, "Held group post retry after roster failed");
                            }
                            event_bus.emit_domain(DomainEvent::AnonGroupMemberListReceived {
                                group_id: anon_group_id,
                                members: members.clone(),
                            });
                            refresh_roster_avatars(anon_service, event_bus, anon_group_id, members)
                                .await
                        }
                        Ok(Ok(false)) => {}
                        Ok(Err(e)) => tracing::error!(
                            group_id = ?anon_group_id,
                            error = %e,
                            "Failed to handle member list"
                        ),
                        Err(e) => tracing::error!(error = %e, "member list task panicked"),
                    }
                });
            }

            InboxEvent::AnonGroupLeave {
                peer_id,
                group_id,
                timestamp,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    group_id = ?group_id,
                    timestamp = timestamp,
                    "Received anonymous group leave notification"
                );

                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupLeave");
                            return;
                        }
                    };

                let anon_service = self.anon_group_service.clone();
                let peer_id_clone = *peer_id;
                let timestamp = *timestamp;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        anon_service.handle_leave_ordered(anon_group_id, peer_id_clone, timestamp)
                    {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            peer_id = %peer_id_clone,
                            error = %e,
                            "Failed to handle leave"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupKick {
                peer_id,
                group_id,
                kicked_user_id,
                admin_signature,
                timestamp,
            } => {
                tracing::info!(
                    peer_id = %peer_id,
                    group_id = ?group_id,
                    kicked_user_id = ?kicked_user_id,
                    timestamp = timestamp,
                    "Received anonymous group kick notification"
                );

                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupKick");
                            return;
                        }
                    };

                let kicked_user = match UserId::try_from_slice(kicked_user_id) {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Invalid kicked_user_id for AnonGroupKick");
                        return;
                    }
                };

                let anon_service = self.anon_group_service.clone();
                let admin_signature_clone = admin_signature.clone();
                let peer_id_clone = *peer_id;
                let timestamp = *timestamp;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = anon_service.handle_kick(
                        anon_group_id,
                        kicked_user,
                        peer_id_clone,
                        &admin_signature_clone,
                        timestamp,
                    ) {
                        tracing::error!(
                            group_id = ?anon_group_id,
                            error = %e,
                            "Failed to handle kick"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupUpdate {
                peer_id,
                group_id,
                update,
                timestamp,
            } => {
                let anon_group_id =
                    match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
                        Some(id) => id,
                        None => {
                            tracing::warn!("Invalid group_id length for AnonGroupUpdate");
                            return;
                        }
                    };

                tracing::info!(
                    peer_id = %peer_id,
                    group_id = %anon_group_id,
                    timestamp = timestamp,
                    "Received anonymous group update"
                );

                let anon_service = self.anon_group_service.clone();
                let update_clone = update.clone();
                let peer_id_clone = *peer_id;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        anon_service.handle_update(anon_group_id, peer_id_clone, &update_clone)
                    {
                        tracing::error!(
                            group_id = %anon_group_id,
                            error = %e,
                            "Failed to handle update"
                        );
                    }
                });
            }

            InboxEvent::AnonGroupMessage {
                peer_id,
                group_id,
                inner_content,
            } => {
                self.handle_anon_group_message(*peer_id, group_id, inner_content);
            }

            // =================================================================
            // Anonymous DM Events
            _ => unreachable!("event routed to wrong inbox concern"),
        }
    }
    fn handle_anon_group_invite(
        &self,
        peer_id: UserId,
        sender_device_id: u32,
        group_id: &[u8],
        group_name: &str,
        inviter_mailbox_pk: &[u8],
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        avatar: &Option<crate::proto::content_v1::AttachmentPointer>,
    ) {
        // A secret group: no inviter, group or name in the log.
        tracing::info!(
            has_profile = profile_key.is_some(),
            "Received anonymous group invite"
        );

        let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
            Some(id) => id,
            None => {
                tracing::warn!(
                    group_id_len = group_id.len(),
                    "Invalid group_id length for AnonGroupInvite (expected 16 bytes)"
                );
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match inviter_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!(
                    mailbox_pk_len = inviter_mailbox_pk.len(),
                    "Invalid mailbox_pk length for AnonGroupInvite (expected 32 bytes)"
                );
                return;
            }
        };

        // Parse and decrypt profile data
        let profile_key_arr: Option<[u8; 32]> = profile_key.as_ref().and_then(|pk| {
            if pk.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(pk);
                Some(key)
            } else {
                None
            }
        });

        use crate::crypto::profile_crypto::decrypt_display_name;

        let inviter_display_name: Option<String> = match (&profile_key_arr, encrypted_display_name)
        {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_display_name(pk, encrypted)
                    .ok()
                    .filter(|n| !n.is_empty())
            }
            _ => None,
        };

        tracing::debug!(
            has_display_name = inviter_display_name.is_some(),
            "Decrypted inviter profile for group invite"
        );

        // Persist before the source envelope is acknowledged. Avatar retrieval is optional.
        let anon_service = self.anon_group_service.clone();
        let event_bus = self.event_bus.clone();
        let avatar = avatar.clone();
        let group_name = group_name.to_string();
        let invite = crate::storage::repositories::anon_group_repo::PendingAnonGroupInvite {
            group_id: anon_group_id,
            group_name: group_name.clone(),
            inviter_id: peer_id,
            inviter_device_id: sender_device_id,
            inviter_mailbox_pk: mailbox_pk,
            profile_key: profile_key_arr,
            inviter_display_name: inviter_display_name.clone(),
            inviter_avatar: None,
        };
        if let Err(error) = anon_service.save_pending_invite(&invite) {
            tracing::error!(%error, "Failed to persist anonymous group invitation");
            return;
        }
        event_bus.emit_domain(DomainEvent::AnonGroupInviteReceived {
            group_id: anon_group_id,
            group_name: group_name.clone(),
            inviter_id: peer_id,
            inviter_device_id: sender_device_id,
            inviter_mailbox_pk: mailbox_pk,
            profile_key: profile_key_arr,
            inviter_display_name: inviter_display_name.clone(),
            inviter_avatar: None,
        });
        tokio::spawn(async move {
            if avatar.is_some() {
                if let Ok(Some(inviter_avatar)) = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    anon_service.resolve_member_avatar(avatar.as_ref(), None),
                )
                .await
                {
                    if anon_service
                        .update_pending_invite_avatar(&invite.group_id, &inviter_avatar)
                        .is_ok()
                    {
                        event_bus.emit_domain(DomainEvent::AnonGroupInviteReceived {
                            group_id: anon_group_id,
                            group_name,
                            inviter_id: peer_id,
                            inviter_device_id: sender_device_id,
                            inviter_mailbox_pk: mailbox_pk,
                            profile_key: profile_key_arr,
                            inviter_display_name,
                            inviter_avatar: Some(inviter_avatar),
                        });
                    }
                }
            }
        });
    }

    fn handle_anon_group_join_ack(
        &self,
        peer_id: UserId,
        sender_device_id: u32,
        group_id: &[u8],
        joiner_mailbox_pk: &[u8],
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        avatar: &Option<crate::proto::content_v1::AttachmentPointer>,
        timestamp: u64,
    ) {
        // A secret group: no joiner or group in the log.
        tracing::info!(
            has_profile = profile_key.is_some(),
            "Received anonymous group join acknowledgment"
        );

        let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid group_id length for AnonGroupJoinAck");
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match joiner_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!("Invalid mailbox_pk length for AnonGroupJoinAck");
                return;
            }
        };

        // Parse and decrypt profile data
        let profile_key_arr: Option<[u8; 32]> = profile_key.as_ref().and_then(|pk| {
            if pk.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(pk);
                Some(key)
            } else {
                None
            }
        });

        use crate::crypto::profile_crypto::decrypt_display_name;

        let display_name: Option<String> = match (&profile_key_arr, encrypted_display_name) {
            (Some(pk), Some(encrypted)) if !encrypted.is_empty() => {
                decrypt_display_name(pk, encrypted)
                    .ok()
                    .filter(|n| !n.is_empty())
            }
            _ => None,
        };
        let avatar = avatar.clone();

        tracing::debug!(
            has_display_name = display_name.is_some(),
            "Decrypted profile for join ack"
        );

        let anon_service = self.anon_group_service.clone();
        let peer_id_clone = peer_id;
        let event_bus = self.event_bus.clone();

        tokio::spawn(async move {
            let avatar = anon_service
                .resolve_member_avatar(avatar.as_ref(), None)
                .await;
            match anon_service
                .handle_join_ack_ordered(
                    anon_group_id,
                    peer_id_clone,
                    sender_device_id,
                    mailbox_pk,
                    profile_key_arr,
                    display_name,
                    avatar,
                    timestamp,
                )
                .await
            {
                // The joiner is in our roster now: their posts that beat
                // this ack in are delivered.
                Ok(joined) => {
                    if joined {
                        event_bus.emit_domain(DomainEvent::AnonGroupMemberJoined {
                            group_id: anon_group_id,
                            user_id: peer_id_clone,
                            profile_key: profile_key_arr,
                            display_name: None,
                            avatar: None,
                        });
                    }
                    for event in anon_service.release_held_posts(&anon_group_id) {
                        event_bus.emit_domain(event);
                    }
                }
                Err(e) => tracing::error!(error = %e, "Failed to handle join ack"),
            }
        });
    }

    fn handle_anon_group_message(
        &self,
        peer_id: UserId,
        group_id: &[u8],
        inner_content: &crate::proto::content_v1::Content,
    ) {
        let anon_group_id = match crate::domain::anon_group::AnonGroupId::try_from_slice(group_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid group_id length for AnonGroupMessage");
                return;
            }
        };

        tracing::debug!(
            peer_id = %peer_id,
            group_id = %anon_group_id,
            "Received anonymous group message"
        );

        // Delegate to service layer (mirrors handle_anon_dm_message pattern).
        let anon_group_service = self.anon_group_service.clone();
        let event_bus = self.event_bus.clone();
        let inner_content_ref = inner_content.clone();

        let _ = tokio::task::spawn_blocking(move || {
            match anon_group_service.process_incoming_content(
                &anon_group_id,
                &peer_id,
                &inner_content_ref,
            ) {
                Ok(events) => {
                    let applied_roster = events.iter().find_map(|event| match event {
                        DomainEvent::AnonGroupMemberListReceived { group_id, members }
                            if *group_id == anon_group_id =>
                        {
                            Some(members.clone())
                        }
                        _ => None,
                    });
                    for event in events {
                        if let DomainEvent::AnonGroupMemberListReceived { group_id, members } =
                            &event
                        {
                            tokio::spawn(refresh_roster_avatars(
                                anon_group_service.clone(),
                                event_bus.clone(),
                                *group_id,
                                members.clone(),
                            ));
                        }
                        event_bus.emit_domain(event);
                    }
                    if let Some(members) = applied_roster {
                        tokio::spawn(async move {
                            if let Err(error) = anon_group_service
                                .drain_pending_after_roster(&anon_group_id)
                                .await
                            {
                                tracing::warn!(%error, "Pending group post drain after roster failed");
                            }
                            if let Err(error) = anon_group_service
                                .retry_held_posts_for_group(&anon_group_id)
                                .await
                            {
                                tracing::warn!(%error, "Held group post retry after roster failed");
                            }
                            // Re-read the local row after fan-out settles.
                            event_bus.emit_domain(DomainEvent::AnonGroupMemberListReceived {
                                group_id: anon_group_id,
                                members,
                            });
                        });
                    }
                }
                Err(e) => {
                    tracing::error!(
                        group_id = %anon_group_id,
                        error = %e,
                        "[ANON_GROUP] Failed to process incoming message"
                    );
                }
            }
        });
    }
}

/// Fetch the avatars a member list points at (over Tor, skipping ones we
/// already hold) and store the changed ones; when any changed, announce the
/// roster again so an open view re-reads it. Runs after the roster is applied
/// so a slow fetch never delays membership. One attempt each first, so a
/// failing fetch never holds up the others; the failed ones are then retried
/// with backoff (a flaky Tor circuit, RC107).
async fn refresh_roster_avatars(
    service: std::sync::Arc<crate::services::anon_group::AnonGroupService>,
    event_bus: EventBus,
    group_id: crate::domain::anon_group::AnonGroupId,
    members: Vec<crate::proto::content_v1::AnonGroupMemberInfo>,
) {
    use crate::services::secret_avatar::RETRY_DELAYS;
    let mut pending = members.clone();
    for delays in [&RETRY_DELAYS[..1], &RETRY_DELAYS[1..]] {
        match service
            .refresh_roster_avatars(&group_id, &pending, delays)
            .await
        {
            Ok((changed, failed)) => {
                if changed {
                    event_bus.emit_domain(DomainEvent::AnonGroupMemberListReceived {
                        group_id,
                        members: members.clone(),
                    });
                }
                if failed.is_empty() {
                    return;
                }
                pending = failed;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to store secret group avatars");
                return;
            }
        }
    }
}
