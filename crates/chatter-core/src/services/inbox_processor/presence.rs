use super::*;

impl InboxEventProcessor {
    pub(super) async fn process_presence(&self, event: &InboxEvent) {
        match event {
            // Typing indicators — bridge inbox events to TypingStarted/Stopped
            // domain events (consumed by events/handlers.rs → chat state → UI).
            // action: 1 = Started, else Stopped. DM typing is keyed by the
            // peer's derived channel_id (matches the stored ServerDm rows).
            // Our own typing, fanned to our other devices along with the
            // members': it is not "someone is typing" here.
            InboxEvent::DmTyping {
                peer_id: user_id, ..
            }
            | InboxEvent::ChannelTyping {
                sender_id: user_id, ..
            } if self.message_service.current_user_id().ok().as_ref() == Some(user_id) => {}

            InboxEvent::DmTyping {
                peer_id,
                action,
                timestamp,
            } => {
                let channel_id = crate::domain::ids::ChannelId::from(*peer_id);
                let (user_id, timestamp) = (*peer_id, *timestamp);
                self.event_bus.emit_domain(if *action == 1 {
                    DomainEvent::TypingStarted {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                } else {
                    DomainEvent::TypingStopped {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                });
            }

            InboxEvent::ChannelTyping {
                channel_id,
                sender_id,
                action,
                timestamp,
            } => {
                let (user_id, channel_id, timestamp) = (*sender_id, *channel_id, *timestamp);
                self.event_bus.emit_domain(if *action == 1 {
                    DomainEvent::TypingStarted {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                } else {
                    DomainEvent::TypingStopped {
                        user_id,
                        channel_id,
                        timestamp,
                    }
                });
            }

            InboxEvent::DmReceipt {
                peer_id,
                sender_device_id,
                receipt_type,
                timestamps,
            } => {
                if let Err(error) = self.message_service.apply_peer_receipt(
                    crate::domain::ids::ChannelId::from(*peer_id),
                    crate::domain::message::ConversationType::ServerDm,
                    *peer_id,
                    crate::domain::ids::DeviceId(*sender_device_id),
                    *receipt_type,
                    timestamps,
                ) {
                    tracing::warn!(%error, "Failed to persist DM receipt");
                }
            }

            // =================================================================
            // Friend-request fan-out — translate the inbox-layer notification
            // into a UI-visible DomainEvent. Without this bridge the user
            // events stream fires `InboxEvent::FriendRequestsUpdated` but the
            // UI (which only listens to `DomainEvent`) never refreshes the
            // pending-requests list.
            // =================================================================
            InboxEvent::FriendRequestsUpdated { received, sent } => {
                self.event_bus
                    .emit_domain(DomainEvent::FriendRequestsUpdated {
                        received: received.clone(),
                        sent: sent.clone(),
                    });
            }

            InboxEvent::FriendAccepted { friend_user_id } => {
                match friend_user_id.parse::<UserId>() {
                    Ok(friend_id) => {
                        // DMs are channel-free (Signal-style): there is no
                        // server DM channel to fetch. The sidebar is built
                        // from the friends list (Task 3.6), so the DM only
                        // appears once the friendship row exists. The accepter
                        // wrote it inline; we (the original requester) learn of
                        // acceptance here, so we must persist it ourselves.
                        if let Err(e) = self.user_service.record_accepted_friend_local(&friend_id) {
                            tracing::warn!(
                                peer_id = %friend_id,
                                error = %e,
                                "FriendAccepted: failed to persist friendship locally — DM will not appear"
                            );
                        }
                        self.event_bus
                            .emit_domain(DomainEvent::FriendAccepted { friend_id });
                        // Resolve their name now rather than at the next
                        // profile fetch (OPEN-ITEMS §119). Without their key
                        // yet this keeps the stored name; their handshake then
                        // brings the key and refreshes again.
                        self.spawn_peer_profile_refresh(friend_id);

                        // Establish the DM Signal session in the background now
                        // that we're friends, so the first call offer / message
                        // doesn't pay the first-contact Sesame bootstrap on the
                        // call path. No-op on the higher-UUID side (Sesame).
                        let ms = self.message_service.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ms.prewarm_dm_session(&friend_id).await {
                                tracing::warn!(
                                    peer_id = %friend_id,
                                    error = %e,
                                    "FriendAccepted: prewarm DM session failed"
                                );
                            }
                        });

                        // Reply with our profile key once we hold theirs.
                        // The accepter sends on acceptance; sending before
                        // their key arrives would expose this first DM to the
                        // server as an identified send.
                        let ms = self.message_service.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                ms.send_profile_key_handshake_if_sealed(&friend_id).await
                            {
                                tracing::warn!(
                                    peer_id = %friend_id,
                                    error = %e,
                                    "FriendAccepted: profile-key handshake not delivered; retries exhausted or no longer friends"
                                );
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            friend_user_id,
                            "FriendAccepted: failed to parse friend user ID — UI will not refresh"
                        );
                    }
                }
            }

            // The gateway or DM inbox stored a new sender profile key.
            // Refresh their profile and, for an accepted friend, return our
            // key now that a sealed send can use theirs. A key already held
            // does not emit this event for pure handshakes, so replies stop.
            InboxEvent::ProfileKeyReceived { sender_id } => {
                self.spawn_peer_profile_refresh(*sender_id);
                // The accepted-friend guard also handles a key arriving
                // before FriendAccepted: that event will send once it has
                // persisted the friendship.
                let ms = self.message_service.clone();
                let peer_id = *sender_id;
                tokio::spawn(async move {
                    if let Err(e) = ms.send_profile_key_handshake_if_sealed(&peer_id).await {
                        tracing::warn!(
                            peer_id = %peer_id,
                            error = %e,
                            "Incoming profile key: reply handshake not delivered"
                        );
                    }
                });
            }

            // =================================================================
            // Everything else has no protocol work. Nothing but this loop
            // reads InboxEvents, so an event that needs the UI must be given
            // an arm above that emits a DomainEvent.
            // =================================================================
            _ => unreachable!("event routed to wrong inbox concern"),
        }
    }

    /// Fetch + decrypt a peer's profile (`get_user_profile` upserts it
    /// locally) and emit it as `UserProfileUpdated`, the event the UI already
    /// handles for sidebar / header / friends-list refresh.
    pub(super) fn spawn_peer_profile_refresh(&self, peer_id: UserId) {
        let user_service = self.user_service.clone();
        let event_bus = self.event_bus.clone();
        tokio::spawn(async move {
            match user_service.get_user_profile(&peer_id).await {
                Ok(profile) => {
                    event_bus.emit_domain(DomainEvent::UserProfileUpdated {
                        user_id: peer_id,
                        display_name: profile.display_name,
                        status_message: profile.status_message.as_ref().map(|s| s.text.clone()),
                        avatar_bytes: profile.avatar_bytes,
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        peer_id = %peer_id,
                        error = %e,
                        "failed to fetch/decrypt peer profile"
                    );
                }
            }
        });
    }
}
