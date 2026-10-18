use super::*;

impl ChatterClient {
    // ── Events ──────────────────────────────────────────────────────────

    pub fn event_bus(&self) -> &EventBus {
        &self.services.event_bus
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<DomainEvent> {
        self.services.event_bus.subscribe_domain()
    }

    // ── Typed event streams (UI-facing, narrow) ─────────────────────────
    //
    // UI crates must not call `subscribe_domain()` directly. These helpers
    // filter the raw DomainEvent bus into focused payload types from
    // [`crate::client::events`] so each UI site only sees the variants it
    // cares about. Each call spawns a dedicated fan-out task tied to the
    // subscription's lifetime (tasks exit when the receiver is dropped and
    // the next filter attempt fails to publish).

    /// Subscribe to anonymous DM request / acceptance / init events.
    ///
    /// Filters `DomainEvent::AnonDmRequestReceived`, `AnonDmRequestAccepted`,
    /// and `AnonDmInitReceived` into the typed [`events::AnonDmRequestEvent`].
    pub fn subscribe_anon_dm_requests(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::AnonDmRequestEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::AnonDmRequestEvent>(32);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::AnonDmRequestReceived {
                        conversation_id,
                        sender_id,
                        sender_mailbox_pk,
                        intro_message,
                        profile_key,
                        sender_display_name,
                        sender_avatar,
                    }) => {
                        if tx
                            .send(events::AnonDmRequestEvent::Received {
                                conversation_id,
                                sender_id,
                                sender_mailbox_pk,
                                intro_message,
                                profile_key,
                                sender_display_name,
                                sender_avatar,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(DomainEvent::AnonDmRequestAccepted {
                        conversation_id,
                        peer_id,
                        peer_mailbox_pk,
                        profile_key,
                        peer_display_name,
                        peer_avatar,
                    }) => {
                        if tx
                            .send(events::AnonDmRequestEvent::Accepted {
                                conversation_id,
                                peer_id,
                                peer_mailbox_pk,
                                profile_key,
                                peer_display_name,
                                peer_avatar,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(DomainEvent::AnonDmInitReceived {
                        conversation_id,
                        sender_id,
                        sender_device_id: _,
                        sender_display_name,
                        sender_avatar,
                        peer_mailbox_pk,
                        peer_identity_key,
                        peer_delivery_token,
                        intro_message,
                        server_timestamp,
                    }) => {
                        if tx
                            .send(events::AnonDmRequestEvent::Init {
                                conversation_id,
                                sender_id,
                                sender_display_name,
                                sender_avatar,
                                peer_mailbox_pk,
                                peer_identity_key,
                                peer_delivery_token,
                                intro_message,
                                server_timestamp,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_anon_dm_requests lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Subscribe to anonymous group invite events.
    pub fn subscribe_anon_group_invites(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::AnonGroupInviteEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::AnonGroupInviteEvent>(16);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::AnonGroupInviteReceived {
                        group_id,
                        group_name,
                        inviter_id,
                        inviter_device_id,
                        inviter_mailbox_pk,
                        profile_key,
                        inviter_display_name,
                        inviter_avatar,
                    }) => {
                        if tx
                            .send(events::AnonGroupInviteEvent {
                                group_id,
                                group_name,
                                inviter_id,
                                inviter_device_id,
                                inviter_mailbox_pk,
                                profile_key,
                                inviter_display_name,
                                inviter_avatar,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_anon_group_invites lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Subscribe to call key exchange events (PFS answer + group key distribution).
    pub fn subscribe_call_key_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::CallKeyEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::CallKeyEvent>(16);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::CallKeyAnswerReceived {
                        call_id,
                        peer_id,
                        ephemeral_public_key,
                    }) => {
                        if tx
                            .send(events::CallKeyEvent::AnswerReceived {
                                call_id,
                                peer_id,
                                ephemeral_public_key,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_call_key_events lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Subscribe to remote call hangup events (peer ended the call).
    pub fn subscribe_remote_call_hangups(
        &self,
    ) -> tokio::sync::broadcast::Receiver<events::RemoteCallHangup> {
        let (tx, rx) = tokio::sync::broadcast::channel::<events::RemoteCallHangup>(16);
        let mut src = self.services.event_bus.subscribe_domain();
        tokio::spawn(async move {
            loop {
                match src.recv().await {
                    Ok(DomainEvent::CallHangupReceived {
                        call_id,
                        peer_id,
                        reason,
                    }) => {
                        if tx
                            .send(events::RemoteCallHangup {
                                call_id,
                                peer_id,
                                reason,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscribe_remote_call_hangups lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// Record a keystroke in `conversation`. The core state machine decides
    /// whether this fires a new STARTED, resets the pause timer, or is
    /// throttled. UI callers must NOT manage typing timers themselves.
    pub fn typing_bump(self: &Arc<Self>, conversation: crate::state::ActiveConversation) {
        // Skip typing-to-self (DM mirrors are irrelevant signal noise).
        if let crate::state::ActiveConversation::ServerDm { peer_id } = &conversation {
            if let Some(user_id) = self.services.auth_state.get().current_user_id {
                if peer_id == &user_id {
                    return;
                }
            }
        }
        if matches!(conversation, crate::state::ActiveConversation::None) {
            return;
        }
        let handle = self.conversation(conversation);
        self.outgoing_typing.bump(handle);
    }

    /// User stopped typing explicitly (compose cleared, message sent, ESC).
    /// Sends STOPPED and cancels timers if typing was active.
    pub fn typing_clear(self: &Arc<Self>) {
        let client = Arc::clone(self);
        self.outgoing_typing
            .clear(move |conversation| Some(client.conversation(conversation.clone())));
    }

}
