//! Channel event service for handling member events and system messages.
//!
//! Centralizes system message creation for member join/leave events,
//! separating business logic from UI handlers.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::channel::Membership;
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent, SystemMessage};
use crate::error::StorageError;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::MessageRepository;
use crate::storage::repositories::user_repo::UserRepository;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::current_user::CurrentUserProvider;
use crate::proto::chat_v1::channel_event;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::error::NetworkError;
use crate::services::channel::membership_log::{Applied, LogObserver, MembershipLog, MembershipNotice};
use crate::network::streaming::heartbeat::{self, Next};

#[derive(Debug, thiserror::Error)]
pub enum ChannelEventError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("grpc error: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("network error: {0}")]
    Network(#[from] NetworkError),
}

/// Service for handling channel events (member join/leave) and creating system messages.
/// 
/// This service now owns the gRPC stream and handles all reconnection logic,
/// converting proto events to domain events for the UI to consume.
pub struct ChannelEventService {
    message_repo: Arc<MessageRepository>,
    channel_repo: Arc<ChannelRepository>,
    user_repo: Arc<UserRepository>,
    grpc_pool: Arc<GrpcClientPool>,
    event_bus: EventBus,
    /// Needed to tell "this moderation action targets ME" from "it targets
    /// somebody else" — a ban on us has to drop our own membership.
    settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
    /// Pending invites the UI has already been told about, so a reconnect
    /// refetch announces only the ones it missed.
    announced_invites: parking_lot::Mutex<std::collections::HashSet<ChannelId>>,
    /// Verifies and commits every relayed `ChannelChange`; tells this
    /// service (as its [`LogObserver`]) what to display.
    membership_log: Arc<MembershipLog>,
}

/// Keep `announced` equal to the server's current pending set and return the
/// entries not announced before. Pruning to the pending set means an invite
/// that was answered or expired can be announced again if it is re-sent.
fn unannounced_invites<T>(
    announced: &mut std::collections::HashSet<ChannelId>,
    pending: Vec<(ChannelId, T)>,
) -> Vec<(ChannelId, T)> {
    announced.retain(|id| pending.iter().any(|(p, _)| p == id));
    pending.into_iter().filter(|(id, _)| announced.insert(*id)).collect()
}

impl ChannelEventService {
    /// Create a new ChannelEventService.
    pub fn new(
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        user_repo: Arc<UserRepository>,
        grpc_pool: Arc<GrpcClientPool>,
        event_bus: EventBus,
        settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        membership_log: Arc<MembershipLog>,
    ) -> Self {
        Self {
            message_repo,
            channel_repo,
            user_repo,
            grpc_pool,
            event_bus,
            settings_repo,
            announced_invites: Default::default(),
            membership_log,
        }
    }
    
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
                let healthy = connected_at
                    .is_some_and(|t: std::time::Instant| t.elapsed() >= heartbeat::HEALTHY_STREAM_AFTER);
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
        let mut stream = channel_client
            .stream_channel_events(None)
            .await?;
        
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
                        self.handle_stream_event(event_data, &event.channel_id).await;
                    }
                }
                Next::Item(Err(e)) => {
                    tracing::error!("Channel event stream error: {}", e);
                    return Err(ChannelEventError::Stream(e.to_string()));
                }
                Next::Ended => return Ok(()),
                Next::Silent => {
                    return Err(ChannelEventError::Stream("missed its heartbeats".to_string()));
                }
            }
        }
    }
    
    /// `GetChannelChanges` for every server channel we are in.
    fn catch_up_membership_logs(&self) {
        use crate::domain::channel::ChannelType;
        let Ok(me) = self.settings_repo.current_user_id() else { return };
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
            let Ok(inviter) = inv.inviter_id.parse::<uuid::Uuid>() else { continue };
            let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelInviteReceived {
                channel_id,
                inviter_id: UserId::from(inviter),
                message: (!inv.message.is_empty()).then_some(inv.message),
                expires_at: inv.expires_at,
            });
            tracing::info!(%channel_id, "Pending channel invitation announced on reconnect");
        }
    }

    /// Handle a channel event from the stream.
    async fn handle_stream_event(
        &self,
        event: channel_event::Event,
        channel_id: &str,
    ) {
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
                tracing::info!("Channel updated: channel={}, name={}", channel_id, updated.name);
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
                let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelCreated {
                    channel_id,
                    name: created.name.clone(),
                });
                tracing::info!("Channel created: channel={}, name={}", channel_id, created.name);
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
                let moderator_id =
                    UserId::from(action.moderator_id.parse::<uuid::Uuid>().unwrap_or(uuid::Uuid::nil()));
                let action_name = proto_moderation_action_name(action.action);
                let is_self = self
                    .settings_repo
                    .current_user_id()
                    .map(|me| me == user_id)
                    .unwrap_or(false);

                if matches!(action_name, "MUTE" | "UNMUTE") {
                    self.mirror_mute(&channel_id, &user_id, action_name == "MUTE", action.duration.as_ref());
                }

                let reason = (!action.reason.is_empty()).then(|| action.reason.clone());
                let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelModerationAction {
                    channel_id,
                    user_id,
                    moderator_id,
                    action: action_name.to_string(),
                    reason,
                    is_self,
                });
                tracing::info!(
                    "Channel moderation: channel={}, user={}, action={}, self={}",
                    channel_id, user_id, action_name, is_self
                );
            }

            Event::InvitationSent(invite) => {
                let Ok(inviter_uuid) = invite.inviter_id.parse::<uuid::Uuid>() else {
                    tracing::error!("Invitation event with unparseable inviter id");
                    return;
                };
                let message = (!invite.message.is_empty()).then(|| invite.message.clone());
                self.announced_invites.lock().insert(channel_id);
                let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelInviteReceived {
                    channel_id,
                    inviter_id: UserId::from(inviter_uuid),
                    message,
                    expires_at: invite.expires_at,
                });
                tracing::info!("Channel invitation received: channel={}", channel_id);
            }

            Event::InvitationResponse(response) => {
                let Ok(inviter_uuid) = response.inviter_id.parse::<uuid::Uuid>() else {
                    tracing::error!("Invitation response with unparseable inviter id");
                    return;
                };
                let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelInviteResponded {
                    channel_id,
                    inviter_id: UserId::from(inviter_uuid),
                    accepted: response.accepted,
                });
                tracing::info!(
                    "Channel invitation answered: channel={}, accepted={}",
                    channel_id, response.accepted
                );
            }

            Event::OperationStarted(started) => {
                // Purely informational: the mutation RPCs are streams, and this
                // is the "accepted, working on it" tick. Nothing to persist.
                tracing::debug!(
                    "Channel operation started: channel={}, operation={}",
                    channel_id, started.operation_id
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
                    channel_id, failed.operation_id, code, error.message
                );

                self.rollback_channel(&channel_id).await;

                let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelOperationFailed {
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
    fn mirror_mute(&self, channel_id: &ChannelId, user_id: &UserId, muted: bool, duration: Option<&prost_types::Duration>) {
        let Ok(Some(mut membership)) = self.channel_repo.get_membership(channel_id, user_id) else { return };
        membership.is_muted = muted;
        membership.muted_until = duration
            .filter(|_| muted)
            .and_then(|d| chrono::Duration::try_seconds(d.seconds))
            .map(|d| Utc::now() + d);
        if let Err(e) = self.channel_repo.update_membership(&membership) {
            tracing::warn!(%channel_id, %user_id, error = %e, "Failed to mirror mute locally");
            return;
        }
        let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelMemberUpdated {
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
            .send(DomainEvent::ChannelArchivedChanged { channel_id: target, archived });
        tracing::info!(
            "Channel archive state changed: channel={}, archived={}",
            target, archived
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

    /// Handle a member joining a channel.
    ///
    /// Creates a system message and syncs membership to local storage.
    /// Returns (message, display_name) for UI updates, or `None` when this
    /// event's notice is already filed (see [`member_notice_id`]).
    pub fn handle_member_joined(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        membership: &Membership,
        event_ts: u64,
    ) -> Result<Option<(Message, String)>, ChannelEventError> {
        // Sync membership (insert or ignore if already exists)
        self.channel_repo.insert_or_ignore_membership(membership)?;

        let message = self.create_system_message(
            member_notice_id(channel_id, user_id, "joined", event_ts),
            channel_id,
            SystemMessage::UserJoined(*user_id),
            Some(*user_id),
        );
        if !self.message_repo.insert_or_ignore(&message)? {
            return Ok(None);
        }

        Ok(Some((message, self.get_display_name(user_id))))
    }

    /// [`Self::handle_member_joined`], then tell the UI.
    fn publish_member_joined(&self, membership: &Membership, event_ts: u64) {
        let (channel_id, user_id) = (membership.channel_id, membership.user_id);
        let Ok(Some((message, display_name))) = self.handle_member_joined(&channel_id, &user_id, membership, event_ts)
        else {
            return;
        };
        tracing::info!(%channel_id, %user_id, %display_name, "Channel member joined");
        let event = DomainEvent::ChannelMemberJoined { channel_id, user_id, display_name, message };
        if let Err(e) = self.event_bus.domain_sender().send(event) {
            tracing::warn!("Failed to publish ChannelMemberJoined event: {}", e);
        }
    }

    /// A roster sync found `membership` on the server but not locally. The
    /// roster is display only: the user is shown, but unless the verified
    /// state has them ACTIVE they are an unsigned add — ignored for keys and
    /// surfaced (`MembershipLog::apply_unsigned`). The notice is keyed on the
    /// server's `joined_at`, so every sync that sees this join files it once.
    pub fn record_synced_join(&self, membership: &Membership) {
        self.classify_unsigned(&membership.channel_id, &membership.user_id, true);
        self.publish_member_joined(membership, membership_ts(membership));
    }

    fn classify_unsigned(&self, channel_id: &ChannelId, user_id: &UserId, added: bool) {
        use crate::services::channel::verified_state::UnsignedEvent;
        let user_id = user_id.to_string();
        let event = if added { UnsignedEvent::Added { user_id } } else { UnsignedEvent::Removed { user_id } };
        if let Err(e) = self.membership_log.apply_unsigned(channel_id, event) {
            tracing::warn!(%channel_id, error = %e, "Could not apply an unsigned roster change");
        }
    }

    /// A sync found `membership` gone from the server. Unless a signed change
    /// already removed them, this is an unsigned removal: they leave the
    /// distribution set at once (fail-safe) and stay in the verified state
    /// pending a signed change. `reason` is what the server still records
    /// (`GetDeparture`: a ban, else [`MemberRemoval::Unexplained`]). Keyed on
    /// the membership that ended, so every sync that sees it files it once.
    pub fn record_synced_departure(&self, membership: &Membership, reason: MemberRemoval) {
        self.classify_unsigned(&membership.channel_id, &membership.user_id, false);
        self.publish_member_removed(
            &membership.channel_id,
            &membership.user_id,
            reason,
            membership_ts(membership),
        );
    }

    /// The channel was deleted, heard live or found by a sync: we are out of
    /// it, but it stays listed read-only with its history and "{owner} deleted
    /// the channel", as Signal keeps an ended group. One notice per channel
    /// however many times it is heard. A channel never stored here is ignored.
    pub fn record_channel_deleted(&self, channel_id: &ChannelId) {
        let (Ok(Some(channel)), Ok(me)) = (self.channel_repo.get(channel_id), self.settings_repo.current_user_id())
        else {
            return;
        };
        if let Err(e) = self.apply_channel_deleted(&channel, &me) {
            tracing::warn!(%channel_id, error = %e, "Failed to record channel deletion");
        }
    }

    fn apply_channel_deleted(&self, channel: &crate::domain::channel::Channel, me: &UserId) -> Result<(), ChannelEventError> {
        match self.channel_repo.delete_membership(&channel.id, me) {
            Ok(()) | Err(StorageError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.channel_repo.mark_former(&channel.id, true)?;
        let id = MessageId(uuid::Uuid::new_v5(
            &uuid::Uuid::nil(),
            format!("channel_deleted_notice:{}", channel.id).as_bytes(),
        ));
        let message = self.create_system_message(id, &channel.id, SystemMessage::ChannelDeleted, Some(channel.owner_id));
        if !self.message_repo.insert_or_ignore(&message)? {
            return Ok(());
        }
        tracing::info!(channel_id = %channel.id, "Channel deleted");
        let event = DomainEvent::ChannelMemberLeft {
            channel_id: channel.id,
            user_id: *me,
            display_name: self.get_display_name(me),
            message,
        };
        if let Err(e) = self.event_bus.domain_sender().send(event) {
            tracing::warn!("Failed to publish channel deletion: {}", e);
        }
        Ok(())
    }

    /// The owner handed the channel to `to` (a verified
    /// `TransferOwnershipAction`): record the new owner and roles and file
    /// "{from} made {to} the owner" once per change.
    fn record_ownership_transferred(&self, channel_id: &ChannelId, from: &UserId, to: &UserId, event_ts: u64) {
        if let Err(e) = crate::services::channel::apply_ownership_transfer(&self.channel_repo, channel_id, from, to) {
            tracing::warn!(%channel_id, error = %e, "Failed to record ownership transfer");
            return;
        }
        let content = if self.settings_repo.current_user_id().ok() == Some(*to) {
            SystemMessage::MadeYouOwner
        } else {
            SystemMessage::OwnershipTransferred { to: *to, to_name: self.known_name(to) }
        };
        let id = member_notice_id(channel_id, to, "owner", event_ts);
        let message = self.create_system_message(id, channel_id, content, Some(*from));
        match self.message_repo.insert_or_ignore(&message) {
            Ok(true) => {
                if let Err(e) = self.event_bus.domain_sender().send(DomainEvent::MessageReceived { message }) {
                    tracing::warn!("Failed to publish ownership notice: {}", e);
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(%channel_id, error = %e, "Failed to file ownership notice"),
        }
    }

    /// Handle a member leaving/being removed from a channel.
    ///
    /// Creates a system message and removes membership from local storage.
    /// Returns (message, display_name) for UI updates, or `None` when this
    /// event's notice is already filed (see [`member_notice_id`]).
    pub fn handle_member_removed(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: MemberRemoval,
        event_ts: u64,
    ) -> Result<Option<(Message, String)>, ChannelEventError> {
        let display_name = self.get_display_name(user_id);
        let is_self = self.settings_repo.current_user_id().ok() == Some(*user_id);

        // `sender_id` is the subject of the rendered "{sender} {description}"
        // line: the leaver for a leave, the moderator for a removal.
        let (content, subject) = match reason {
            MemberRemoval::Left => (SystemMessage::UserLeft(*user_id), *user_id),
            MemberRemoval::Kicked(by) if is_self => (SystemMessage::SelfRemoved { by }, by),
            MemberRemoval::Banned(by) if is_self => (SystemMessage::SelfBanned { by }, by),
            MemberRemoval::Unexplained if is_self => (SystemMessage::NoLongerMember, *user_id),
            MemberRemoval::Unexplained => (SystemMessage::UserLeft(*user_id), *user_id),
            MemberRemoval::Kicked(by) => (
                SystemMessage::UserRemoved { by, removed: *user_id, removed_name: self.known_name(user_id) },
                by,
            ),
            MemberRemoval::Banned(by) => (
                SystemMessage::UserBanned { by, banned: *user_id, banned_name: self.known_name(user_id) },
                by,
            ),
        };
        let id = member_notice_id(channel_id, user_id, "removed", event_ts);
        let message = self.create_system_message(id, channel_id, content, Some(subject));

        // Remove membership from local storage. Already gone is fine: our own
        // leave deletes it first, and a roster never fetched has no row.
        match self.channel_repo.delete_membership(channel_id, user_id) {
            Ok(()) | Err(StorageError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        // Ours: the channel stays listed, read-only, as Signal keeps a group
        // you left or were removed from.
        if is_self {
            self.channel_repo.mark_former(channel_id, false)?;
        }

        // Our own leave reaches us twice with unrelated timestamps: the leave
        // RPC (`record_own_leave`) and the server's broadcast on the channel
        // stream we are still subscribed to. Neither can key off the other.
        if is_self && reason == MemberRemoval::Left && self.recently_recorded_own_leave(channel_id, user_id) {
            return Ok(None);
        }

        if !self.message_repo.insert_or_ignore(&message)? {
            return Ok(None);
        }

        Ok(Some((message, display_name)))
    }

    /// [`Self::handle_member_removed`], then tell the UI, which appends the
    /// notice to the open chat.
    fn publish_member_removed(&self, channel_id: &ChannelId, user_id: &UserId, reason: MemberRemoval, event_ts: u64) {
        let Ok(Some((message, display_name))) = self.handle_member_removed(channel_id, user_id, reason, event_ts)
        else {
            return;
        };
        tracing::info!(%channel_id, %user_id, %display_name, "Channel member left");
        let event = DomainEvent::ChannelMemberLeft { channel_id: *channel_id, user_id: *user_id, display_name, message };
        if let Err(e) = self.event_bus.domain_sender().send(event) {
            tracing::warn!("Failed to publish ChannelMemberLeft event: {}", e);
        }
    }

    /// File "You left the channel" after our own leave succeeded. The server
    /// only broadcasts the departure to the members who remain, so this
    /// device never hears it on the event stream.
    pub fn record_own_leave(&self, channel_id: &ChannelId) {
        match self.settings_repo.current_user_id() {
            Ok(me) => self.publish_member_removed(channel_id, &me, MemberRemoval::Left, 0),
            Err(e) => tracing::warn!(%channel_id, error = %e, "No current user; leave notice not filed"),
        }
    }

    /// Whether our newest membership notice in `channel_id` is a leave filed
    /// in the last two minutes: then a second own-leave is the same one.
    // time window, because the RPC and the broadcast carry no shared
    // id; a server-issued departure id on both would make this exact.
    fn recently_recorded_own_leave(&self, channel_id: &ChannelId, me: &UserId) -> bool {
        let conv_type = crate::services::message::conversion::conversation_type_for_channel(&self.channel_repo, channel_id);
        let Ok(recent) = self.message_repo.list(channel_id, &conv_type, None, 20) else { return false };
        let newest_own = recent.iter().find_map(|m| match &m.content {
            MessageContent::System(SystemMessage::UserJoined(u)) if u == me => Some(None),
            MessageContent::System(SystemMessage::UserLeft(u)) if u == me => Some(Some(m.created_at)),
            _ => None,
        });
        matches!(newest_own, Some(Some(at)) if Utc::now() - at < chrono::Duration::minutes(2))
    }

    /// Get display name for a user, falling back to user ID string.
    fn get_display_name(&self, user_id: &UserId) -> String {
        match self.user_repo.get(user_id) {
            Ok(Some(user)) => user.username,
            Ok(None) => user_id.to_string(),
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to get user for display name, falling back to ID"
                );
                user_id.to_string()
            }
        }
    }

    /// A real name for `user_id` from the local stores — profile display name,
    /// then a handle — in `UserService::get_display_name`'s order. `None` when
    /// only the id-placeholder username is known, so the caller can word the
    /// notice generically instead of printing a UUID.
    fn known_name(&self, user_id: &UserId) -> Option<String> {
        if let Ok(Some(profile)) = self.user_repo.get_profile(user_id) {
            if !profile.display_name.is_empty() {
                return Some(profile.display_name);
            }
        }
        self.user_repo
            .get(user_id)
            .ok()
            .flatten()
            .map(|user| user.username)
            .filter(|name| !name.is_empty() && *name != user_id.to_string())
    }

    /// Create a system message for the given channel.
    fn create_system_message(
        &self,
        id: MessageId,
        channel_id: &ChannelId,
        content: SystemMessage,
        subject: Option<UserId>,
    ) -> Message {
        let now = Utc::now();
        // `sender_id` drives the UI's name lookup — the converter formats
        // system messages as `"{sender_name} {description}"`, e.g.
        // "Alice joined the channel". Set it to the user the event is
        // about (joiner / leaver / kicked) so `get_display_name` can
        // resolve a real name instead of falling back to "Anonymous".
        Message {
            id,
            conversation_id: *channel_id,
            // The channel's own bucket, as its messages are filed: anything
            // else is missing from its history and sidebar preview.
            conversation_type: crate::services::message::conversion::conversation_type_for_channel(
                &self.channel_repo,
                channel_id,
            ),
            sender_id: subject,
            device_id: None,
            content: MessageContent::System(content),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
            created_at: now,
            updated_at: now,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
            failure_reason: None,
        }
    }
}

/// Display of verified membership changes: the roster rows, the member
/// notices and the domain events the UI reacts to. Key decisions never read
/// any of this; they read the verified state `MembershipLog` just committed.
impl LogObserver for ChannelEventService {
    fn applied(&self, a: &Applied) {
        use crate::domain::channel::NotificationPreference;
        use crate::proto::chat_v1::{channel_action::Action, remove_member::Reason};
        use crate::services::channel::converters::proto_role_to_domain;
        use crate::services::channel::verified_state::MemberStatus;

        let parse = |u: &str| u.parse::<uuid::Uuid>().ok().map(UserId::from);
        let Some(author) = parse(&a.author) else { return };
        let ts = a.timestamp;
        let active = |s: Option<&crate::services::channel::verified_state::MembershipState>, u: &str| {
            s.and_then(|s| s.members.get(u)).filter(|m| m.status == MemberStatus::Active).map(|m| m.role)
        };
        let genesis = a.actions.iter().any(|x| matches!(x.action, Some(Action::Genesis(_))));
        let notify = !a.backfill;

        // Departures, with the signed reason.
        if let Some(prev) = &a.prev {
            for (uid, _) in prev.active_members() {
                if active(Some(&a.new), uid).is_some() {
                    continue;
                }
                let Some(user) = parse(uid) else { continue };
                let reason = a
                    .actions
                    .iter()
                    .find_map(|x| match &x.action {
                        Some(Action::RemoveMember(r)) if r.user_id == *uid => Some(r.reason),
                        _ => None,
                    })
                    .map_or(MemberRemoval::Unexplained, |r| match Reason::try_from(r) {
                        Ok(Reason::Kicked) => MemberRemoval::Kicked(author),
                        Ok(Reason::Banned) => MemberRemoval::Banned(author),
                        _ => MemberRemoval::Left,
                    });
                if notify {
                    self.publish_member_removed(&a.channel_id, &user, reason, ts);
                } else if let Err(e) = self.channel_repo.delete_membership(&a.channel_id, &user) {
                    tracing::debug!(channel_id = %a.channel_id, %user, error = %e, "No roster row to drop");
                }
            }
        }

        // Arrivals, and role changes of those who stayed.
        for (uid, m) in a.new.active_members() {
            let Some(user) = parse(uid) else { continue };
            let role = proto_role_to_domain(m.role as i32);
            match active(a.prev.as_ref(), uid) {
                None => {
                    let membership = Membership {
                        channel_id: a.channel_id,
                        user_id: user,
                        role,
                        notification_preference: NotificationPreference::All,
                        joined_at: chrono::DateTime::from_timestamp_nanos(ts as i64),
                        last_read_message_id: None,
                        is_muted: false,
                        muted_until: None,
                    };
                    if notify && !(genesis && user == author) {
                        self.publish_member_joined(&membership, ts);
                    } else if let Err(e) = self.channel_repo.insert_or_ignore_membership(&membership) {
                        tracing::warn!(channel_id = %a.channel_id, %user, error = %e, "Failed to store member");
                    }
                }
                Some(old) if old != m.role => {
                    if let Ok(Some(mut row)) = self.channel_repo.get_membership(&a.channel_id, &user) {
                        row.role = role;
                        if let Err(e) = self.channel_repo.update_membership(&row) {
                            tracing::warn!(channel_id = %a.channel_id, %user, error = %e, "Failed to store role change");
                        }
                    }
                    let _ = self.event_bus.domain_sender().send(DomainEvent::ChannelMemberUpdated {
                        channel_id: a.channel_id,
                        user_id: user,
                        role,
                    });
                }
                Some(_) => {}
            }
        }

        for x in &a.actions {
            if let Some(Action::TransferOwnership(t)) = &x.action {
                if let Some(to) = parse(&t.new_owner_user_id) {
                    if notify {
                        self.record_ownership_transferred(&a.channel_id, &author, &to, ts);
                    } else if let Err(e) =
                        crate::services::channel::apply_ownership_transfer(&self.channel_repo, &a.channel_id, &author, &to)
                    {
                        tracing::warn!(channel_id = %a.channel_id, error = %e, "Failed to record ownership transfer");
                    }
                }
            }
        }
    }

    fn notice(&self, channel_id: &ChannelId, notice: MembershipNotice) {
        let _ = self
            .event_bus
            .domain_sender()
            .send(DomainEvent::ChannelMembershipNotice { channel_id: *channel_id, notice });
    }
}

/// The member notice's primary key, derived from the server event it records.
///
/// The server publishes one membership `ChannelEvent` to the channel's stream
/// AND to every member's `user:{id}:channel_events`, and the global stream is
/// subscribed to both, so a member receives the same event twice (same bytes,
/// same `timestamp`). With a deterministic id `insert_or_ignore` files it once
/// however many paths deliver it. A zero timestamp carries no identity, so it
/// gets a fresh id rather than colliding with every other such event.
fn member_notice_id(channel_id: &ChannelId, user_id: &UserId, kind: &str, event_ts: u64) -> MessageId {
    if event_ts == 0 {
        return MessageId::new();
    }
    let key = format!("channel_member_notice:{channel_id}:{user_id}:{kind}:{event_ts}");
    MessageId(uuid::Uuid::new_v5(&uuid::Uuid::nil(), key.as_bytes()))
}

/// A membership's server `joined_at` in nanoseconds: the identity of a join
/// or departure a roster sync found, standing in for the missed event's time.
fn membership_ts(membership: &Membership) -> u64 {
    membership.joined_at.timestamp_nanos_opt().unwrap_or(0) as u64
}

/// Why a member dropped off the roster: the signed `RemoveMember.reason`, or
/// what a roster sync can still learn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRemoval {
    Left,
    Kicked(UserId),
    Banned(UserId),
    /// Gone from the roster with no reason on record: what a sync that missed
    /// the event is left with after a kick (the server keeps none) or a leave.
    /// Another member's reads as a leave; ours as "no longer a member".
    Unexplained,
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::chat_v1::{
        ChannelError as ProtoChannelError, ChannelModeration, OperationFailed,
    };
    use crate::domain::message::ConversationType;
    use crate::storage::Database;

    #[test]
    fn a_reconnect_refetch_announces_only_missed_invites() {
        let (a, b) = (ChannelId::new(), ChannelId::new());
        let mut announced = std::collections::HashSet::from([a]); // `a` came live
        let missed = unannounced_invites(&mut announced, vec![(a, ()), (b, ())]);
        assert_eq!(missed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![b]);
        assert!(unannounced_invites(&mut announced, vec![(a, ()), (b, ())]).is_empty());

        // `a` answered, then re-sent: announced again.
        assert!(unannounced_invites(&mut announced, vec![(b, ())]).is_empty());
        assert_eq!(unannounced_invites(&mut announced, vec![(a, ()), (b, ())]).len(), 1);
    }

    fn setup() -> (Arc<ChannelEventService>, EventBus, Arc<ChannelRepository>) {
        let db = crate::services::channel::membership_log::test_support::test_db();
        let (service, bus, repo, _) = setup_on(db, &crate::services::channel::membership_log::test_support::who(0xfe), &Default::default(), &[]);
        (service, bus, repo)
    }

    /// The service on `db`, its membership log signing as `me` against
    /// `server`, registered as the log's observer.
    fn setup_on(
        db: Arc<Database>,
        me: &crate::services::channel::membership_log::test_support::Who,
        server: &Arc<crate::services::channel::membership_log::test_support::FakeServer>,
        people: &[&crate::services::channel::membership_log::test_support::Who],
    ) -> (Arc<ChannelEventService>, EventBus, Arc<ChannelRepository>, Arc<MembershipLog>) {
        let channel_repo = Arc::new(ChannelRepository::new(db.clone()));
        let event_bus = EventBus::new();
        // Never reachable: the rollback fetch is expected to fail, which is
        // the "server no longer has this channel" path.
        let grpc_pool = Arc::new(
            crate::network::grpc::client_pool::GrpcClientPool::new("https://127.0.0.1:1").unwrap(),
        );
        let (log, _, _) = crate::services::channel::membership_log::test_support::device_on(db.clone(), me, server, people);

        let service = Arc::new(ChannelEventService::new(
            Arc::new(MessageRepository::new(db.clone())),
            channel_repo.clone(),
            Arc::new(UserRepository::new(db.clone())),
            grpc_pool,
            event_bus.clone(),
            Arc::new(crate::storage::repositories::settings_repo::SettingsRepository::new(db)),
            log.clone(),
        ));
        log.set_observer(Arc::downgrade(&(service.clone() as Arc<dyn LogObserver>)));

        (service, event_bus, channel_repo, log)
    }

    /// The keepalive changes nothing and says nothing, even with the empty
    /// channel_id an all-channels subscription sends it with.
    #[tokio::test]
    async fn a_heartbeat_changes_nothing() {
        let (service, bus, _) = setup();
        let mut rx = bus.domain_sender().subscribe();
        service
            .handle_stream_event(
                channel_event::Event::Heartbeat(crate::proto::chat_v1::StreamHeartbeat {}),
                "",
            )
            .await;
        assert!(rx.try_recv().is_err(), "a heartbeat produced a domain event");
    }

    /// INFRA-29 regression: `OperationFailed` is the saga-compensation signal.
    /// It used to fall into `_ => debug!("Unhandled channel event type")`, so a
    /// Temporal channel workflow could roll back and the client would keep its
    /// optimistic state forever, with nothing on screen and nothing in the log.
    #[tokio::test]
    async fn operation_failed_reaches_the_ui_layer() {
        let (service, bus, _) = setup();
        let mut rx = bus.domain_sender().subscribe();
        let channel_id = ChannelId::from(uuid::Uuid::new_v4());

        service
            .handle_stream_event(
                channel_event::Event::OperationFailed(OperationFailed {
                    operation_id: "op-123".to_string(),
                    channel_id: channel_id.to_string(),
                    user_id: uuid::Uuid::new_v4().to_string(),
                    error: Some(ProtoChannelError {
                        code: crate::proto::chat_v1::channel_error::Code::PermissionDenied as i32,
                        message: "not allowed".to_string(),
                        details: Default::default(),
                    }),
                }),
                &channel_id.to_string(),
            )
            .await;

        match rx.try_recv() {
            Ok(DomainEvent::ChannelOperationFailed {
                channel_id: got,
                operation_id,
                code,
                message,
            }) => {
                assert_eq!(got, channel_id);
                assert_eq!(operation_id, "op-123");
                assert_eq!(code, "PERMISSION_DENIED");
                assert_eq!(message, "not allowed");
            }
            other => panic!("OperationFailed did not reach the domain bus: {:?}", other),
        }
    }

    /// A deleted channel stays listed read-only (Signal keeps an ended group):
    /// our membership goes, the channel is marked deleted, one "{owner}
    /// deleted the channel" is filed however often the deletion is heard, and
    /// the UI is told as for any departure of ours.
    #[tokio::test]
    async fn channel_deleted_keeps_the_channel_read_only_with_one_notice() {
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();
        let me = as_me(&service);
        let channel_id = private_channel(&channel_repo);
        let own = Membership {
            channel_id,
            user_id: me,
            role: crate::domain::channel::MemberRole::Member,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        };
        channel_repo.insert_or_ignore_membership(&own).unwrap();

        let deleted = || channel_event::Event::ChannelDeleted(channel_id.to_string());
        service.handle_stream_event(deleted(), &channel_id.to_string()).await;
        service.record_channel_deleted(&channel_id); // the sync finding it too

        assert!(channel_repo.get(&channel_id).unwrap().is_some(), "history must stay");
        assert!(channel_repo.get_membership(&channel_id, &me).unwrap().is_none());
        assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(true));
        assert_eq!(channel_repo.list_former_for_user(&me).unwrap().len(), 1, "not listed read-only");
        assert_eq!(service.message_repo.count(&channel_id, &ConversationType::Channel).unwrap(), 1);
        match rx.try_recv() {
            Ok(DomainEvent::ChannelMemberLeft { user_id, message, .. }) => {
                assert_eq!(user_id, me);
                assert!(matches!(message.content, MessageContent::System(SystemMessage::ChannelDeleted)));
            }
            other => panic!("expected ChannelMemberLeft, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "the second hearing reached the UI");

        // A later leave notice for the same channel does not undo "deleted".
        channel_repo.mark_former(&channel_id, false).unwrap();
        assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(true));

        // A channel never stored here has nothing to keep listed.
        let unknown = ChannelId::from(uuid::Uuid::new_v4());
        channel_repo.mark_former(&unknown, false).unwrap();
        assert_eq!(channel_repo.former_state(&unknown).unwrap(), None);
    }

    use crate::proto::chat_v1::{self as pb, channel_action::Action, remove_member::Reason as RmReason};
    use crate::services::channel::membership_log::test_support::{who, FakeServer, Who};
    use crate::services::channel::membership_log::AuthorBase;
    use crate::services::channel::verified_state::{ChannelRole, Policy};

    fn act(a: Action) -> pb::ChannelAction {
        pb::ChannelAction { action: Some(a) }
    }

    /// A private channel owned by `owner` on a fake server, and this
    /// device's event service logged in as `me`, an ACTIVE member.
    struct World {
        server: Arc<FakeServer>,
        owner: Who,
        me: Who,
        other: Who,
        o_log: Arc<MembershipLog>,
        my_log: Arc<MembershipLog>,
        service: Arc<ChannelEventService>,
        bus: EventBus,
        channel_repo: Arc<ChannelRepository>,
        ch: ChannelId,
    }

    impl World {
        async fn new() -> Self {
            let (owner, me, other) = (who(1), who(2), who(3));
            let people = [&owner, &me, &other];
            let server = Arc::new(FakeServer::default());
            let db = crate::services::channel::membership_log::test_support::test_db();
            let (service, bus, channel_repo, my_log) = setup_on(db, &me, &server, &people);
            service
                .settings_repo
                .set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &me.id)
                .unwrap();
            let (o_log, _, _) = crate::services::channel::membership_log::test_support::device(&owner, &server, &people);
            let ch = ChannelId::from(uuid::Uuid::new_v4());
            let channel = crate::domain::channel::Channel::new(
                ch,
                "verified".into(),
                crate::domain::channel::ChannelType::Private,
                UserId::from(owner.id.parse::<uuid::Uuid>().unwrap()),
            );
            channel_repo.upsert(&channel).unwrap();
            let w = World { server, owner, me, other, o_log, my_log, service, bus, channel_repo, ch };
            w.by_owner(vec![act(Action::Genesis(pb::Genesis {
                policy: Some(Policy::defaults(false).to_proto()),
                disappearing_seconds: 0,
            }))])
            .await;
            w.by_owner(vec![w.add(&w.me)]).await;
            w.relay_last().await;
            w.author(&w.my_log, vec![act(Action::AcceptInvite(pb::AcceptInvite {}))]).await;
            w
        }

        fn add(&self, who: &Who) -> pb::ChannelAction {
            act(Action::AddMember(pb::AddMember {
                user_id: who.id.clone(),
                role: ChannelRole::Member as i32,
                identity_key: who.key().to_vec(),
                pending: true,
            }))
        }

        async fn author(&self, log: &MembershipLog, acts: Vec<pb::ChannelAction>) {
            let base = if acts.iter().any(|a| matches!(a.action, Some(Action::Genesis(_)))) {
                AuthorBase::Genesis
            } else {
                AuthorBase::Verified
            };
            log.author(&self.ch, base, None, |_| Ok(acts.clone()), |c| {
                let r = self.server.append(&self.ch, c);
                async move { r }
            })
            .await
            .unwrap();
        }

        async fn by_owner(&self, acts: Vec<pb::ChannelAction>) {
            self.o_log.catch_up(&self.ch).await.unwrap();
            self.author(&self.o_log, acts).await;
        }

        fn last(&self) -> pb::ChannelChange {
            self.server.log.lock().last().unwrap().clone()
        }

        async fn relay(&self, change: pb::ChannelChange) {
            self.service.handle_stream_event(channel_event::Event::ChannelChange(change), &self.ch.to_string()).await;
        }

        async fn relay_last(&self) {
            self.relay(self.last()).await;
        }

        fn count(&self) -> u64 {
            self.service.message_repo.count(&self.ch, &ConversationType::Channel).unwrap()
        }

        fn uid(w: &Who) -> UserId {
            UserId::from(w.id.parse::<uuid::Uuid>().unwrap())
        }
    }

    /// F77 on the verified path: the server relays one change on two paths
    /// (channel stream + per-user stream). It is applied once, files one
    /// notice and tells the UI once; a later change is a new notice.
    #[tokio::test]
    async fn a_membership_change_delivered_twice_files_one_notice() {
        let w = World::new().await;
        let mut rx = w.bus.domain_sender().subscribe();
        let base = w.count();

        // Other is invited (no notice) and accepts: "joined".
        w.by_owner(vec![w.add(&w.other)]).await;
        let (x_log, _, _) = crate::services::channel::membership_log::test_support::device(
            &w.other,
            &w.server,
            &[&w.owner, &w.me, &w.other],
        );
        w.author(&x_log, vec![act(Action::AcceptInvite(pb::AcceptInvite {}))]).await;
        let accept = w.last();
        for _ in 0..2 {
            w.relay(accept.clone()).await;
        }
        assert_eq!(w.count(), base + 1, "one join filed two notices");
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberJoined { .. })));
        assert!(rx.try_recv().is_err(), "the duplicate reached the UI");
        assert!(w.channel_repo.get_membership(&w.ch, &World::uid(&w.other)).unwrap().is_some());

        // Other leaves: "left", once.
        w.author(&x_log, vec![act(Action::RemoveMember(pb::RemoveMember { user_id: w.other.id.clone(), reason: RmReason::Left as i32 }))]).await;
        let leave = w.last();
        for _ in 0..2 {
            w.relay(leave.clone()).await;
        }
        assert_eq!(w.count(), base + 2, "one leave filed two notices");
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberLeft { .. })));
        assert!(rx.try_recv().is_err(), "the duplicate reached the UI");
        assert!(w.channel_repo.get_membership(&w.ch, &World::uid(&w.other)).unwrap().is_none());
    }

    /// A signed kick is shown as a kick by its author, and the kicked
    /// member's roster row goes.
    #[tokio::test]
    async fn a_verified_kick_names_its_moderator() {
        let w = World::new().await;
        w.by_owner(vec![w.add(&w.other)]).await;
        let (x_log, _, _) = crate::services::channel::membership_log::test_support::device(
            &w.other,
            &w.server,
            &[&w.owner, &w.me, &w.other],
        );
        w.author(&x_log, vec![act(Action::AcceptInvite(pb::AcceptInvite {}))]).await;
        w.by_owner(vec![act(Action::RemoveMember(pb::RemoveMember { user_id: w.other.id.clone(), reason: RmReason::Kicked as i32 }))]).await;
        let mut rx = w.bus.domain_sender().subscribe();
        w.relay_last().await; // gap: catches up the accept, then the kick
        let notices = w.service.message_repo.list(&w.ch, &ConversationType::Channel, None, 20).unwrap();
        assert!(notices.iter().any(|m| matches!(&m.content,
            MessageContent::System(SystemMessage::UserRemoved { by, removed, .. })
                if *by == World::uid(&w.owner) && *removed == World::uid(&w.other))));
        let mut left = false;
        while let Ok(ev) = rx.try_recv() {
            left |= matches!(ev, DomainEvent::ChannelMemberLeft { .. });
        }
        assert!(left);
        assert!(!w.my_log.load(&w.ch).unwrap().unwrap().state.members.contains_key(&w.other.id));
    }

    /// The old unsigned path is gone: a roster sync naming a user nobody in
    /// the channel signed in shows them, but they never enter the verified
    /// distribution set, and the user is told.
    #[tokio::test]
    async fn an_unsigned_roster_add_is_ignored_for_keys_and_surfaced() {
        let w = World::new().await;
        let mut rx = w.bus.domain_sender().subscribe();
        let ghost = UserId::from(uuid::Uuid::new_v4());
        let row = Membership {
            channel_id: w.ch,
            user_id: ghost,
            role: crate::domain::channel::MemberRole::Admin,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        };
        w.service.record_synced_join(&row);
        let v = w.my_log.load(&w.ch).unwrap().unwrap();
        use crate::services::channel::verified_state::VerifiedMembership;
        assert!(!v.contains(&ghost.to_string()));
        let mut surfaced = false;
        while let Ok(ev) = rx.try_recv() {
            surfaced |= matches!(ev, DomainEvent::ChannelMembershipNotice {
                notice: MembershipNotice::UnverifiedMember { .. }, ..
            });
        }
        assert!(surfaced, "the unsigned add was not surfaced");
    }

    /// Our own leave files "left the channel" even though the leave already
    /// deleted our membership row.
    #[tokio::test]
    async fn own_leave_files_a_notice_without_a_membership_row() {
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();
        let me = UserId::from(uuid::Uuid::new_v4());
        service
            .settings_repo
            .set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        let channel = crate::domain::channel::Channel::new(
            ChannelId::from(uuid::Uuid::new_v4()),
            "invtest".to_string(),
            crate::domain::channel::ChannelType::Private,
            UserId::from(uuid::Uuid::new_v4()),
        );
        channel_repo.upsert(&channel).unwrap();

        service.record_own_leave(&channel.id);

        // Filed where the channel's history and sidebar preview read.
        assert_eq!(service.message_repo.count(&channel.id, &ConversationType::Channel).unwrap(), 1);
        match rx.try_recv() {
            Ok(DomainEvent::ChannelMemberLeft { user_id, message, .. }) => {
                assert_eq!(user_id, me);
                assert_eq!(message.sender_id, Some(me));
                assert!(matches!(message.content, MessageContent::System(SystemMessage::UserLeft(u)) if u == me));
            }
            other => panic!("expected ChannelMemberLeft, got {other:?}"),
        }
    }

    fn as_me(service: &ChannelEventService) -> UserId {
        let me = UserId::from(uuid::Uuid::new_v4());
        service
            .settings_repo
            .set_string(crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        me
    }

    fn private_channel(channel_repo: &ChannelRepository) -> ChannelId {
        let channel = crate::domain::channel::Channel::new(
            ChannelId::from(uuid::Uuid::new_v4()),
            "invtest".to_string(),
            crate::domain::channel::ChannelType::Private,
            UserId::from(uuid::Uuid::new_v4()),
        );
        channel_repo.upsert(&channel).unwrap();
        channel.id
    }

    /// F79: our own leave arrives as our own committed change and from the
    /// leave RPC's `record_own_leave`, in either order. One "You left" and
    /// one UI event.
    #[tokio::test]
    async fn own_leave_seen_on_both_paths_files_one_notice() {
        let w = World::new().await;
        let mut rx = w.bus.domain_sender().subscribe();
        let base = w.count();
        w.author(&w.my_log, vec![act(Action::RemoveMember(pb::RemoveMember { user_id: w.me.id.clone(), reason: RmReason::Left as i32 }))]).await;
        w.relay_last().await; // the server's echo: a duplicate
        w.service.record_own_leave(&w.ch);
        assert_eq!(w.count(), base + 1, "our leave filed two notices");
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberLeft { .. })));
        assert!(rx.try_recv().is_err(), "the duplicate reached the UI");
        assert_eq!(w.channel_repo.former_state(&w.ch).unwrap(), Some(false));
    }

    /// F80: our own Leave (the RPC path: `leave_channel` deletes our
    /// membership, then `record_own_leave`) keeps the channel listed
    /// read-only — former, not deleted — with our "left the channel" notice.
    /// RC20 saw a left channel vanish, but that channel was left before F80.
    #[tokio::test]
    async fn own_leave_keeps_the_channel_listed_read_only() {
        let (service, _bus, channel_repo) = setup();
        let me = as_me(&service);
        let channel_id = private_channel(&channel_repo);
        let own = Membership {
            channel_id,
            user_id: me,
            role: crate::domain::channel::MemberRole::Member,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        };
        channel_repo.insert_or_ignore_membership(&own).unwrap();

        channel_repo.delete_membership(&channel_id, &me).unwrap();
        service.record_own_leave(&channel_id);

        assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(false));
        let listed = channel_repo.list_former_for_user(&me).unwrap();
        assert_eq!(listed.iter().map(|c| c.id).collect::<Vec<_>>(), [channel_id], "the left channel vanished");
        let notices = service.message_repo.list(&channel_id, &ConversationType::Channel, None, 10).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].content, MessageContent::System(SystemMessage::UserLeft(me)));
        assert_eq!(
            crate::domain::channel::SendBlock::for_viewer(None, false, Some(false)),
            Some(crate::domain::channel::SendBlock::Removed),
            "a left channel must replace the composer, not accept sends"
        );
    }

    /// F79: a roster sync that finds a member gone (or new) files the notice
    /// the missed live event would have, once however many syncs see it.
    #[tokio::test]
    async fn synced_roster_changes_file_one_notice_each() {
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();
        let _me = as_me(&service);
        let channel_id = private_channel(&channel_repo);
        let count = || service.message_repo.count(&channel_id, &ConversationType::Channel).unwrap();
        let member = Membership {
            channel_id,
            user_id: UserId::from(uuid::Uuid::new_v4()),
            role: crate::domain::channel::MemberRole::Member,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        };

        service.record_synced_join(&member);
        service.record_synced_join(&member);
        assert_eq!(count(), 1);
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberJoined { .. })));
        assert!(rx.try_recv().is_err());

        service.record_synced_departure(&member, MemberRemoval::Unexplained);
        service.record_synced_departure(&member, MemberRemoval::Unexplained);
        assert_eq!(count(), 2);
        match rx.try_recv() {
            Ok(DomainEvent::ChannelMemberLeft { user_id, message, .. }) => {
                assert_eq!(user_id, member.user_id);
                assert!(matches!(message.content, MessageContent::System(SystemMessage::UserLeft(u)) if u == member.user_id));
            }
            other => panic!("expected ChannelMemberLeft, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert!(channel_repo.get_membership(&channel_id, &member.user_id).unwrap().is_none());
    }

    /// F80: our own removal found by a sync reads as what the server still
    /// records — "{mod} banned you" for a ban, "You are no longer a member"
    /// with no record — and the channel stays listed read-only.
    #[tokio::test]
    async fn synced_own_departure_keeps_the_channel_with_its_reason() {
        let (service, _bus, channel_repo) = setup();
        let me = as_me(&service);
        let moderator = UserId::from(uuid::Uuid::new_v4());
        for (reason, expect) in [
            (MemberRemoval::Banned(moderator), SystemMessage::SelfBanned { by: moderator }),
            (MemberRemoval::Unexplained, SystemMessage::NoLongerMember),
        ] {
            let channel_id = private_channel(&channel_repo);
            let own = Membership {
                channel_id,
                user_id: me,
                role: crate::domain::channel::MemberRole::Member,
                notification_preference: crate::domain::channel::NotificationPreference::All,
                joined_at: Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
            };
            channel_repo.insert_or_ignore_membership(&own).unwrap();

            service.record_synced_departure(&own, reason);

            assert!(channel_repo.get_membership(&channel_id, &me).unwrap().is_none());
            assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(false));
            let notices = service.message_repo.list(&channel_id, &ConversationType::Channel, None, 10).unwrap();
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].content, MessageContent::System(expect));
        }
        assert_eq!(channel_repo.list_former_for_user(&me).unwrap().len(), 2);
    }

    /// Mute is an unsigned server action (routing only): it is mirrored onto
    /// the roster row and reaches the UI; the member stays a member.
    #[tokio::test]
    async fn moderation_mute_is_mirrored_and_announced() {
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();
        let me = as_me(&service);
        let channel_id = private_channel(&channel_repo);
        channel_repo
            .insert_membership(&Membership {
                channel_id,
                user_id: me,
                role: crate::domain::channel::MemberRole::Member,
                notification_preference: crate::domain::channel::NotificationPreference::All,
                joined_at: Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
            })
            .unwrap();

        service
            .handle_stream_event(
                channel_event::Event::ModerationAction(ChannelModeration {
                    channel_id: channel_id.to_string(),
                    user_id: me.to_string(),
                    moderator_id: uuid::Uuid::new_v4().to_string(),
                    action: crate::proto::chat_v1::channel_moderation::Action::Mute as i32,
                    duration: Some(prost_types::Duration { seconds: 600, nanos: 0 }),
                    reason: "spam".to_string(),
                }),
                &channel_id.to_string(),
            )
            .await;

        let row = channel_repo.get_membership(&channel_id, &me).unwrap().expect("still a member");
        assert!(row.is_muted);
        assert!(row.muted_until.is_some());
        let mut moderation = None;
        while let Ok(ev) = rx.try_recv() {
            if let DomainEvent::ChannelModerationAction { action, is_self, reason, .. } = ev {
                moderation = Some((action, is_self, reason));
            }
        }
        let (action, is_self, reason) = moderation.expect("ModerationAction did not reach the domain bus");
        assert_eq!(action, "MUTE");
        assert!(is_self);
        assert_eq!(reason.as_deref(), Some("spam"));
    }
}
