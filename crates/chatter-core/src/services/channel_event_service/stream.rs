use super::*;

/// Keep `announced` equal to the server's current pending set and return the
/// entries not announced before. Pruning to the pending set means an invite
/// that was answered or expired can be announced again if it is re-sent.
pub(super) fn unannounced_invites<T>(
    announced: &mut std::collections::HashSet<ChannelId>,
    pending: Vec<(ChannelId, T)>,
) -> Vec<(ChannelId, T)> {
    announced.retain(|id| pending.iter().any(|(p, _)| p == id));
    pending
        .into_iter()
        .filter(|(id, _)| announced.insert(*id))
        .collect()
}

impl ChannelEventService {
    /// Start the channel event stream.
    ///
    /// This spawns a background task that owns the gRPC stream,
    /// handles reconnection logic, and emits domain events.
    pub fn start_event_stream(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let mut connected_at = None;
                let outcome = self.run_stream_with_retry(&mut connected_at).await;
                // The server ending a subscription is not the end of it: it used
                // to `break` here, leaving this client without channel events
                // until restart. A stream that had been healthy reconnects at
                // once; failures to (re)connect wait 5 s.
                let healthy = connected_at.is_some_and(|t: std::time::Instant| {
                    t.elapsed() >= heartbeat::HEALTHY_STREAM_AFTER
                });
                match outcome {
                    Ok(()) => tracing::info!("Channel event stream ended; reconnecting"),
                    Err(e) => tracing::warn!("Channel event stream error: {}; reconnecting", e),
                }
                let wait = if healthy {
                    std::time::Duration::from_millis(250)
                } else {
                    std::time::Duration::from_secs(5)
                };
                tokio::time::sleep(wait).await;
            }
        })
    }

    /// Run the event stream with automatic retry on failure.
    async fn run_stream_with_retry(
        &self,
        connected_at: &mut Option<std::time::Instant>,
    ) -> Result<(), ChannelEventError> {
        let mut channel_client = self.grpc_pool.channel_client();

        // Stream events for all channels
        let mut stream = channel_client.stream_channel_events(None).await?;

        *connected_at = Some(std::time::Instant::now());
        tracing::info!("Channel event stream started");

        // Invitations reach an invitee only as a live push on this stream, so
        // one sent while it was down (app backgrounded, reconnecting) was
        // invisible until the next start. Refetch after subscribing, so
        // nothing falls between the fetch and the stream.
        self.announce_missed_invites().await;
        // Membership changes relayed while the stream was down: fetch and
        // verify them now, in the background, ahead of their messages.
        self.catch_up_membership_logs();

        loop {
            match heartbeat::next_or_silent(&mut stream, heartbeat::STREAM_SILENCE_LIMIT).await {
                Next::Item(Ok(event)) => {
                    if let Some(event_data) = event.event {
                        self.handle_stream_event(event_data, &event.channel_id)
                            .await;
                    }
                }
                Next::Item(Err(e)) => {
                    tracing::error!("Channel event stream error: {}", e);
                    return Err(ChannelEventError::Stream(e.to_string()));
                }
                Next::Ended => return Ok(()),
                Next::Silent => {
                    return Err(ChannelEventError::Stream(
                        "missed its heartbeats".to_string(),
                    ));
                }
            }
        }
    }

    /// `GetChannelChanges` for every server channel we are in.
    fn catch_up_membership_logs(&self) {
        use crate::domain::channel::ChannelType;
        let Ok(me) = self.settings_repo.current_user_id() else {
            return;
        };
        let channels: Vec<ChannelId> = match self.channel_repo.list_for_user(&me) {
            Ok(list) => list
                .into_iter()
                .filter(|c| matches!(c.channel_type, ChannelType::Public | ChannelType::Private))
                .map(|c| c.id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "Cannot list channels for membership catch-up");
                return;
            }
        };
        let log = self.membership_log.clone();
        tokio::spawn(async move {
            for channel_id in channels {
                if let Err(e) = log.catch_up(&channel_id).await {
                    tracing::warn!(%channel_id, error = %e, "Membership catch-up failed");
                }
            }
        });
    }

    /// Emit `ChannelInviteReceived` for every pending invite not yet announced.
    async fn announce_missed_invites(&self) {
        let invites = match self.grpc_pool.channel_client().list_pending_invites().await {
            Ok(resp) => resp.invites,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to refetch pending channel invites");
                return;
            }
        };
        let pending: Vec<_> = invites
            .into_iter()
            .filter_map(|inv| Some((inv.channel_id.parse::<uuid::Uuid>().ok()?.into(), inv)))
            .collect();
        let missed = unannounced_invites(&mut self.announced_invites.lock(), pending);
        for (channel_id, inv) in missed {
            let Ok(inviter) = inv.inviter_id.parse::<uuid::Uuid>() else {
                continue;
            };
            let _ = self
                .event_bus
                .domain_sender()
                .send(DomainEvent::ChannelInviteReceived {
                    channel_id,
                    inviter_id: UserId::from(inviter),
                    message: (!inv.message.is_empty()).then_some(inv.message),
                    expires_at: inv.expires_at,
                });
            tracing::info!(%channel_id, "Pending channel invitation announced on reconnect");
        }
    }

    /// Handle a channel event from the stream.
    pub(super) async fn handle_stream_event(&self, event: channel_event::Event, channel_id: &str) {
        use channel_event::Event;

        // The server's keepalive (also its first item once subscribed). It
        // carries no channel_id when subscribed to all channels, so it goes
        // before the parse that would log it as invalid every minute.
        if matches!(event, Event::Heartbeat(_)) {
            tracing::trace!("Channel event stream heartbeat");
            return;
        }

        let channel_id = match channel_id.parse::<uuid::Uuid>() {
            Ok(uuid) => ChannelId::from(uuid),
            Err(e) => {
                tracing::error!("Invalid channel ID {}: {}", channel_id, e);
                return;
            }
        };

        match event {
            // A signed membership change: verified against our own state
            // before anything is applied (MembershipLog -> `applied`).
            Event::ChannelChange(change) => {
                if let Err(e) = self.membership_log.ingest(&channel_id, &change).await {
                    tracing::warn!(%channel_id, error = %e, "Channel membership change not applied");
                }
            }
            Event::ChannelUpdated(proto_channel) => {
                use crate::services::channel::converters::proto_to_domain_channel;

                let updated = match proto_to_domain_channel(&proto_channel) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("Failed to convert updated channel from proto: {}", e);
                        return;
                    }
                };

                // Merge server-authoritative metadata (name/description/icon/
                // config/archived) into the local row, preserving local-only
                // fields (unread_count, last_message_at). If the channel isn't
                // cached locally, upsert the converted record wholesale.
                match self.channel_repo.get(&channel_id) {
                    Ok(Some(mut existing)) => {
                        existing.name = updated.name.clone();
                        existing.description = updated.description.clone();
                        existing.icon = updated.icon.clone();
                        existing.config = updated.config.clone();
                        existing.archived = updated.archived;
                        existing.updated_at = Utc::now();
                        if let Err(e) = self.channel_repo.update(&existing) {
                            tracing::warn!("Failed to persist channel update locally: {}", e);
                            return;
                        }
                    }
                    Ok(None) => {
                        if let Err(e) = self.channel_repo.upsert(&updated) {
                            tracing::warn!("Failed to upsert updated channel locally: {}", e);
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read channel for update: {}", e);
                        return;
                    }
                }

                let event = DomainEvent::ChannelUpdated {
                    channel_id,
                    name: updated.name.clone(),
                    icon: updated.icon.clone(),
                };
                if let Err(e) = self.event_bus.domain_sender().send(event) {
                    tracing::warn!("Failed to publish ChannelUpdated event: {}", e);
                }
                tracing::info!(
                    "Channel updated: channel={}, name={}",
                    channel_id,
                    updated.name
                );
            }
            // ── INFRA-29: the arms that used to fall into a silent
            // `_ => debug!("Unhandled channel event type")`.
            Event::ChannelCreated(proto_channel) => {
                use crate::services::channel::converters::proto_to_domain_channel;
                let created = match proto_to_domain_channel(&proto_channel) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("Failed to convert created channel from proto: {}", e);
                        return;
                    }
                };
                if let Err(e) = self.channel_repo.upsert(&created) {
                    tracing::warn!("Failed to upsert created channel locally: {}", e);
                    return;
                }
                let _ = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::ChannelCreated {
                        channel_id,
                        name: created.name.clone(),
                    });
                tracing::info!(
                    "Channel created: channel={}, name={}",
                    channel_id,
                    created.name
                );
            }

            Event::ChannelDeleted(deleted_id) => {
                // The id in the payload is authoritative; fall back to the
                // envelope's channel_id when the server sent it empty.
                let target = deleted_id
                    .parse::<uuid::Uuid>()
                    .map(ChannelId::from)
                    .unwrap_or(channel_id);
                self.record_channel_deleted(&target);
            }

            Event::ChannelArchived(id) => self.apply_archive_state(&id, channel_id, true),
            Event::ChannelUnarchived(id) => self.apply_archive_state(&id, channel_id, false),

            // Unsigned, routing-only server actions (unban, mute, unmute and
            // a mute's expiry). Kicks and bans arrive as signed changes.
            Event::ModerationAction(action) => {
                let Ok(user_uuid) = action.user_id.parse::<uuid::Uuid>() else {
                    tracing::error!("Moderation event with an unparseable user id");
                    return;
                };
                let user_id = UserId::from(user_uuid);
                // Empty for a mute the server's expiry sweep lifted.
                let moderator_id = UserId::from(
                    action
                        .moderator_id
                        .parse::<uuid::Uuid>()
                        .unwrap_or(uuid::Uuid::nil()),
                );
                let action_name = proto_moderation_action_name(action.action);
                let is_self = self
                    .settings_repo
                    .current_user_id()
                    .map(|me| me == user_id)
                    .unwrap_or(false);

                if matches!(action_name, "MUTE" | "UNMUTE") {
                    self.mirror_mute(
                        &channel_id,
                        &user_id,
                        action_name == "MUTE",
                        action.duration.as_ref(),
                    );
                }

                let reason = (!action.reason.is_empty()).then(|| action.reason.clone());
                let _ = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::ChannelModerationAction {
                        channel_id,
                        user_id,
                        moderator_id,
                        action: action_name.to_string(),
                        reason,
                        is_self,
                    });
                tracing::info!(
                    "Channel moderation: channel={}, user={}, action={}, self={}",
                    channel_id,
                    user_id,
                    action_name,
                    is_self
                );
            }

            Event::InvitationSent(invite) => {
                let Ok(inviter_uuid) = invite.inviter_id.parse::<uuid::Uuid>() else {
                    tracing::error!("Invitation event with unparseable inviter id");
                    return;
                };
                let message = (!invite.message.is_empty()).then(|| invite.message.clone());
                self.announced_invites.lock().insert(channel_id);
                let _ = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::ChannelInviteReceived {
                        channel_id,
                        inviter_id: UserId::from(inviter_uuid),
                        message,
                        expires_at: invite.expires_at,
                    });
                tracing::info!("Channel invitation received: channel={}", channel_id);
            }

            Event::InvitationResponse(response) => {
                self.announced_invites.lock().remove(&channel_id);
                let _ = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::ChannelInviteResponded {
                        channel_id,
                        accepted: response.accepted,
                    });
                tracing::info!(
                    "Channel invitation answered: channel={}, accepted={}",
                    channel_id,
                    response.accepted
                );
            }

            Event::OperationStarted(started) => {
                // Purely informational: the mutation RPCs are streams, and this
                // is the "accepted, working on it" tick. Nothing to persist.
                tracing::debug!(
                    "Channel operation started: channel={}, operation={}",
                    channel_id,
                    started.operation_id
                );
            }

            Event::OperationFailed(failed) => {
                // THE saga-compensation signal. A Temporal channel workflow
                // rolled back; whatever this client optimistically applied is
                // wrong. Re-read the channel from the server (authoritative
                // after compensation) before telling the UI, so by the time it
                // reacts, local storage already matches the server.
                let error = failed.error.unwrap_or_default();
                let code = proto_channel_error_code_name(error.code).to_string();
                tracing::warn!(
                    "Channel operation FAILED (saga compensated): channel={}, operation={}, code={}, message={}",
                    channel_id,
                    failed.operation_id,
                    code,
                    error.message
                );

                self.rollback_channel(&channel_id).await;

                let _ = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::ChannelOperationFailed {
                        channel_id,
                        operation_id: failed.operation_id.clone(),
                        code,
                        message: error.message.clone(),
                    });
            }

            // Returned above, before the channel_id parse.
            Event::Heartbeat(_) => {}
        }
    }

    /// Mirror a server mute onto the local roster row (display only; the
    /// server enforces it). `duration` unset = until lifted.
    fn mirror_mute(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        muted: bool,
        duration: Option<&prost_types::Duration>,
    ) {
        let Ok(Some(mut membership)) = self.channel_repo.get_membership(channel_id, user_id) else {
            return;
        };
        membership.is_muted = muted;
        membership.muted_until = duration
            .filter(|_| muted)
            .and_then(|d| chrono::Duration::try_seconds(d.seconds))
            .map(|d| Utc::now() + d);
        if let Err(e) = self.channel_repo.update_membership(&membership) {
            tracing::warn!(%channel_id, %user_id, error = %e, "Failed to mirror mute locally");
            return;
        }
        let _ = self
            .event_bus
            .domain_sender()
            .send(DomainEvent::ChannelMemberUpdated {
                channel_id: *channel_id,
                user_id: *user_id,
                role: membership.role,
            });
    }

    /// Persist a channel's archived flag and announce it.
    fn apply_archive_state(&self, payload_id: &str, envelope_id: ChannelId, archived: bool) {
        let target = payload_id
            .parse::<uuid::Uuid>()
            .map(ChannelId::from)
            .unwrap_or(envelope_id);

        match self.channel_repo.get(&target) {
            Ok(Some(mut existing)) => {
                existing.archived = archived;
                existing.updated_at = Utc::now();
                if let Err(e) = self.channel_repo.update(&existing) {
                    tracing::warn!("Failed to persist channel archive state: {}", e);
                    return;
                }
            }
            Ok(None) => {
                tracing::debug!("Archive event for a channel we do not have locally");
            }
            Err(e) => {
                tracing::warn!("Failed to read channel for archive event: {}", e);
                return;
            }
        }

        let _ = self
            .event_bus
            .domain_sender()
            .send(DomainEvent::ChannelArchivedChanged {
                channel_id: target,
                archived,
            });
        tracing::info!(
            "Channel archive state changed: channel={}, archived={}",
            target,
            archived
        );
    }

    /// Re-read a channel from the server after a failed (compensated)
    /// operation and make local storage match: update it if it still exists,
    /// remove it if the server no longer has it (a rolled-back creation).
    async fn rollback_channel(&self, channel_id: &ChannelId) {
        use crate::services::channel::converters::proto_to_domain_channel;

        let mut client = self.grpc_pool.channel_client();
        match client.get_channel(&channel_id.to_string()).await {
            Ok(proto) => match proto_to_domain_channel(&proto) {
                Ok(server_state) => {
                    if let Err(e) = self.channel_repo.upsert(&server_state) {
                        tracing::warn!("Failed to write rolled-back channel state: {}", e);
                    }
                }
                Err(e) => tracing::warn!("Failed to convert channel during rollback: {}", e),
            },
            Err(e) => {
                // NotFound is the expected shape of a rolled-back creation.
                tracing::info!(
                    "Channel unavailable after failed operation ({}); removing local copy",
                    e
                );
                if let Err(e) = self.channel_repo.delete_channel_cascade(channel_id) {
                    tracing::warn!("Failed to remove local channel after rollback: {}", e);
                }
            }
        }
    }
}

/// Proto `ChannelModeration.Action` -> stable name for the UI.
fn proto_moderation_action_name(action: i32) -> &'static str {
    use crate::proto::chat_v1::channel_moderation::Action;
    match Action::try_from(action) {
        Ok(Action::Kick) => "KICK",
        Ok(Action::Ban) => "BAN",
        Ok(Action::Mute) => "MUTE",
        Ok(Action::Unban) => "UNBAN",
        Ok(Action::Unmute) => "UNMUTE",
        _ => "UNSPECIFIED",
    }
}

/// Proto `ChannelError.Code` -> stable name for the UI.
fn proto_channel_error_code_name(code: i32) -> &'static str {
    use crate::proto::chat_v1::channel_error::Code;
    match Code::try_from(code) {
        Ok(Code::Unauthorized) => "UNAUTHORIZED",
        Ok(Code::NotFound) => "NOT_FOUND",
        Ok(Code::PermissionDenied) => "PERMISSION_DENIED",
        Ok(Code::InvalidArgument) => "INVALID_ARGUMENT",
        Ok(Code::RateLimited) => "RATE_LIMITED",
        Ok(Code::ChannelFull) => "CHANNEL_FULL",
        Ok(Code::UserBanned) => "USER_BANNED",
        _ => "UNKNOWN",
    }
}
