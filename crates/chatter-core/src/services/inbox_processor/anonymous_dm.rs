use super::*;

impl InboxEventProcessor {
    pub(super) async fn process_anonymous_dm(&self, event: &InboxEvent) {
        match event {
            // =================================================================
            InboxEvent::AnonDmRequest {
                peer_id,
                sender_device_id,
                conversation_id,
                initiator_mailbox_pk,
                intro_message,
                profile_key,
                encrypted_display_name,
                avatar,
                timestamp: _,
            } => {
                self.handle_anon_dm_request(
                    *peer_id,
                    *sender_device_id,
                    conversation_id,
                    initiator_mailbox_pk,
                    intro_message,
                    profile_key,
                    encrypted_display_name,
                    avatar,
                );
                // Return false - UI needs to add sidebar entry
            }

            InboxEvent::AnonDmAccept {
                peer_id,
                sender_device_id,
                conversation_id,
                responder_mailbox_pk,
                profile_key,
                encrypted_display_name,
                avatar,
                timestamp: _,
            } => {
                self.handle_anon_dm_accept(
                    *peer_id,
                    *sender_device_id,
                    conversation_id,
                    responder_mailbox_pk,
                    profile_key,
                    encrypted_display_name,
                    avatar,
                )
                .await;
                // Return false - UI needs to update sidebar entry
            }

            InboxEvent::AnonDmMessage {
                peer_id,
                conversation_id,
                inner_content,
            } => {
                self.handle_anon_dm_message(*peer_id, conversation_id, inner_content);
            }

            // =================================================================
            // Mixed events (protocol part here, DomainEvent for the UI)
            _ => unreachable!("event routed to wrong inbox concern"),
        }
    }
    fn handle_anon_dm_request(
        &self,
        peer_id: UserId,
        sender_device_id: u32,
        conversation_id: &[u8],
        initiator_mailbox_pk: &[u8],
        intro_message: &Option<String>,
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        avatar: &Option<crate::proto::content_v1::AttachmentPointer>,
    ) {
        tracing::info!(
            has_intro = intro_message.is_some(),
            has_encrypted_name = encrypted_display_name.is_some(),
            has_avatar = avatar.is_some(),
            "Received anonymous DM request"
        );

        let anon_dm_id = match crate::domain::anon_dm::AnonDmId::try_from_slice(conversation_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid conversation_id length for AnonDmRequest");
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match initiator_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!(
                    mailbox_pk_len = initiator_mailbox_pk.len(),
                    "Invalid mailbox_pk length for AnonDmRequest (expected 32 bytes)"
                );
                return;
            }
        };

        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let intro_msg = intro_message.clone();
        let profile_key = profile_key.clone();
        let encrypted_display_name = encrypted_display_name.clone();
        let avatar = avatar.clone();

        tokio::spawn(async move {
            let intro_msg_for_event = intro_msg.clone();
            let service_for_handle = anon_dm_service.clone();
            let result = tokio::task::spawn_blocking(move || {
                service_for_handle.handle_request_encrypted(
                    anon_dm_id,
                    peer_id,
                    sender_device_id,
                    mailbox_pk,
                    intro_msg.as_deref(),
                    profile_key,
                    encrypted_display_name,
                    None,
                )
            })
            .await;
            if let (Ok(Ok(_)), Some(pointer)) = (&result, avatar) {
                tokio::spawn(refresh_peer_avatar(
                    anon_dm_service,
                    event_bus.clone(),
                    anon_dm_id,
                    peer_id,
                    pointer,
                    &crate::services::secret_avatar::RETRY_DELAYS,
                ));
            }

            match result {
                Ok(Ok((_dm, profile))) => {
                    // Emit request received event
                    event_bus.emit_domain(DomainEvent::AnonDmRequestReceived {
                        conversation_id: anon_dm_id,
                        sender_id: peer_id,
                        sender_mailbox_pk: mailbox_pk,
                        intro_message: intro_msg_for_event,
                        profile_key: profile.profile_key,
                        sender_display_name: Some(profile.display_name.clone()),
                        sender_avatar: profile.avatar.clone(),
                    });

                    // Also emit profile updated event to ensure UI updates avatars immediately
                    event_bus.emit_domain(DomainEvent::AnonDmProfileUpdated {
                        conversation_id: anon_dm_id,
                        user_id: peer_id,
                        display_name: Some(profile.display_name),
                        avatar: profile.avatar,
                        avatar_url: None,
                        bio: None,
                        timestamp: TimestampNanos::now().as_u64(),
                    });
                }
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "Failed to handle AnonDmRequest");
                }
                Err(e) => {
                    tracing::error!(error = %e, "spawn_blocking panicked in AnonDmRequest handler");
                }
            }
        });
    }

    async fn handle_anon_dm_accept(
        &self,
        peer_id: UserId,
        sender_device_id: u32,
        conversation_id: &[u8],
        responder_mailbox_pk: &[u8],
        profile_key: &Option<Vec<u8>>,
        encrypted_display_name: &Option<Vec<u8>>,
        avatar: &Option<crate::proto::content_v1::AttachmentPointer>,
    ) {
        tracing::info!(
            has_encrypted_display_name = encrypted_display_name.is_some(),
            has_avatar = avatar.is_some(),
            "Received anonymous DM acceptance"
        );

        let anon_dm_id = match crate::domain::anon_dm::AnonDmId::try_from_slice(conversation_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid conversation_id length for AnonDmAccept");
                return;
            }
        };

        let mailbox_pk: [u8; 32] = match responder_mailbox_pk.try_into() {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!(
                    mailbox_pk_len = responder_mailbox_pk.len(),
                    "Invalid mailbox_pk length for AnonDmAccept (expected 32 bytes)"
                );
                return;
            }
        };

        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let client = self.client.clone();
        let profile_key = profile_key.clone();
        let encrypted_display_name = encrypted_display_name.clone();
        let avatar = avatar.clone();

        // Apply acceptance before returning to the ordered inbox event loop.
        // A message deposited immediately after Accept can be in the same poll
        // batch; deferring this write (especially behind a Tor avatar fetch)
        // makes process_incoming_content see Pending and discard that message.
        let service_for_handle = anon_dm_service.clone();
        let result = tokio::task::spawn_blocking(move || {
            service_for_handle.handle_acceptance_encrypted(
                anon_dm_id,
                peer_id,
                sender_device_id,
                mailbox_pk,
                profile_key,
                encrypted_display_name,
                None,
            )
        })
        .await;
        match result {
            Ok(Ok(profile)) => {
                // Emit acceptance event
                event_bus.emit_domain(DomainEvent::AnonDmRequestAccepted {
                    conversation_id: anon_dm_id,
                    peer_id,
                    peer_mailbox_pk: mailbox_pk,
                    profile_key: profile.profile_key,
                    peer_display_name: profile.display_name.clone(),
                    peer_avatar: profile.avatar.clone(),
                });

                // Also emit profile updated event to ensure UI updates avatars immediately
                event_bus.emit_domain(DomainEvent::AnonDmProfileUpdated {
                    conversation_id: anon_dm_id,
                    user_id: peer_id,
                    display_name: Some(profile.display_name),
                    avatar: profile.avatar,
                    avatar_url: None,
                    bio: None,
                    timestamp: TimestampNanos::now().as_u64(),
                });

                if let Some(pointer) = avatar {
                    tokio::spawn(refresh_peer_avatar(
                        anon_dm_service.clone(),
                        event_bus.clone(),
                        anon_dm_id,
                        peer_id,
                        pointer,
                        &crate::services::secret_avatar::RETRY_DELAYS,
                    ));
                }

                // Auto-send our profile (display name + avatar) back to the
                // peer now that the Double Ratchet is established. The peer
                // only saw our ECIES init at request time — this is the
                // first moment we can share richer identity securely.
                // Moved here from the UI layer so the profile sync
                // happens regardless of frontend.
                let our_name = client.auth_state().get().display_name.clone();
                let service = anon_dm_service.clone();
                tokio::spawn(async move {
                    if let Err(e) = service
                        .send_profile_update(&anon_dm_id, our_name, None, None)
                        .await
                    {
                        tracing::warn!(
                            conversation_id = %anon_dm_id,
                            error = %e,
                            "Failed to send profile back to responder after acceptance"
                        );
                    }
                });
            }
            Ok(Err(e)) => {
                tracing::warn!(conversation_id = %anon_dm_id, error = %e, "Failed to handle secret chat acceptance");
            }
            Err(e) => {
                tracing::error!(error = %e, "spawn_blocking panicked in AnonDmAccept handler")
            }
        }
    }

    fn handle_anon_dm_message(
        &self,
        peer_id: UserId,
        conversation_id: &[u8],
        inner_content: &crate::proto::content_v1::Content,
    ) {
        let anon_dm_id = match crate::domain::anon_dm::AnonDmId::try_from_slice(conversation_id) {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid conversation_id length for AnonDmMessage");
                return;
            }
        };

        // The peer ended the chat (or declined our request to it): our copy
        // and mailbox go. Only the chat's
        // own peer can (checked in `handle_peer_end`), and only for the chat
        // whose mailbox this arrived in.
        if let Some(crate::proto::content_v1::content::Content::AnonDmEnd(end)) =
            &inner_content.content
        {
            if end.conversation_id != conversation_id {
                tracing::warn!("[ANON_DM] Dropped an end signal naming another chat");
                return;
            }
            let service = self.anon_dm_service.clone();
            let event_bus = self.event_bus.clone();
            tokio::spawn(async move {
                use crate::services::anon_dm::PeerEnd;
                match service.handle_peer_end(&anon_dm_id, &peer_id).await {
                    Ok(PeerEnd::Ended) => {
                        event_bus.emit_domain(DomainEvent::AnonDmEnded {
                            conversation_id: anon_dm_id,
                        });
                    }
                    Ok(PeerEnd::Declined) => {
                        event_bus.emit_domain(DomainEvent::AnonDmDeclined {
                            conversation_id: anon_dm_id,
                        });
                    }
                    Ok(PeerEnd::NotPeer) => {
                        tracing::info!("[ANON_DM] Dropped an end signal from a non-peer");
                    }
                    Err(e) => tracing::warn!(error = %e, "[ANON_DM] Failed to end secret chat"),
                }
            });
            return;
        }

        if let Some(crate::proto::content_v1::content::Content::AnonDmProfileUpdate(update)) =
            &inner_content.content
            && let Some(pointer) = update.avatar.clone()
        {
            tokio::spawn(refresh_peer_avatar(
                self.anon_dm_service.clone(),
                self.event_bus.clone(),
                anon_dm_id,
                peer_id,
                pointer,
                &crate::services::secret_avatar::RETRY_DELAYS,
            ));
        }

        // Delegate to service layer for business logic
        let anon_dm_service = self.anon_dm_service.clone();
        let event_bus = self.event_bus.clone();
        let inner_content_ref = inner_content.clone();
        let is_data_message = matches!(
            inner_content.content.as_ref(),
            Some(crate::proto::content_v1::content::Content::DataMessage(_))
        );

        let _ = tokio::task::spawn_blocking(move || {
            match anon_dm_service.process_incoming_content(
                &anon_dm_id,
                &peer_id,
                &inner_content_ref,
            ) {
                Ok(events) => {
                    let stored_message = events
                        .iter()
                        .any(|event| matches!(event, DomainEvent::MessageReceived { .. }));
                    for event in events {
                        event_bus.emit_domain(event);
                    }
                    if is_data_message && stored_message {
                        let timestamp = inner_content_ref.timestamp;
                        tokio::spawn(async move {
                            if let Err(e) = anon_dm_service
                                .send_delivery_receipt(&anon_dm_id, vec![timestamp])
                                .await
                            {
                                tracing::warn!(error = %e, "[ANON_DM] Failed to send delivery receipt");
                            }
                        });
                    }
                }
                Err(e) => {
                    tracing::error!(
                        conversation_id = %anon_dm_id,
                        error = %e,
                        "[ANON_DM] Failed to process incoming message"
                    );
                }
            }
        });
    }
}

/// Fetch the avatar a secret-chat control points at (retrying a failed fetch
/// after each of `delays`) and, when it is new, store it and tell the UI. The
/// control itself (name, bio, the chat) was applied without waiting for this.
async fn refresh_peer_avatar(
    service: std::sync::Arc<crate::services::anon_dm::AnonDmService>,
    event_bus: EventBus,
    conversation_id: crate::domain::anon_dm::AnonDmId,
    peer_id: UserId,
    pointer: crate::proto::content_v1::AttachmentPointer,
    delays: &'static [std::time::Duration],
) {
    match service
        .refresh_peer_avatar(&conversation_id, &peer_id, &pointer, delays)
        .await
    {
        Ok(Some(avatar)) => {
            event_bus.emit_domain(DomainEvent::AnonDmProfileUpdated {
                conversation_id,
                user_id: peer_id,
                display_name: None,
                avatar: Some(avatar),
                avatar_url: None,
                bio: None,
                timestamp: TimestampNanos::now().as_u64(),
            });
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "Failed to store secret chat avatar"),
    }
}
