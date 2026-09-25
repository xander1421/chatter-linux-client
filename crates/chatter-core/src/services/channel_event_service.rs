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
use tokio_stream::StreamExt;

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
    ) -> Self {
        Self {
            message_repo,
            channel_repo,
            user_repo,
            grpc_pool,
            event_bus,
            settings_repo,
            announced_invites: Default::default(),
        }
    }
    
    /// Start the channel event stream.
    ///
    /// This spawns a background task that owns the gRPC stream,
    /// handles reconnection logic, and emits domain events.
    pub fn start_event_stream(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.run_stream_with_retry().await {
                    Ok(()) => {
                        tracing::info!("Channel event stream ended normally");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("Channel event stream error: {}. Reconnecting in 5s...", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }
        })
    }
    
    /// Run the event stream with automatic retry on failure.
    async fn run_stream_with_retry(&self) -> Result<(), ChannelEventError> {
        let mut channel_client = self.grpc_pool.channel_client();
        
        // Stream events for all channels
        let mut stream = channel_client
            .stream_channel_events(None)
            .await?;
        
        tracing::info!("Channel event stream started");

        // Invitations reach an invitee only as a live push on this stream, so
        // one sent while it was down (app backgrounded, reconnecting) was
        // invisible until the next start. Refetch after subscribing, so
        // nothing falls between the fetch and the stream.
        self.announce_missed_invites().await;

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    if let Some(event_data) = event.event {
                        self.handle_stream_event(
                            event_data,
                            &event.channel_id,
                            event.timestamp,
                        ).await;
                    }
                }
                Err(e) => {
                    tracing::error!("Channel event stream error: {}", e);
                    return Err(ChannelEventError::Stream(e.to_string()));
                }
            }
        }
        
        Ok(())
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
        timestamp: u64,
    ) {
        use channel_event::Event;
        use crate::domain::channel::{MemberRole, NotificationPreference};
        
        let channel_id = match channel_id.parse::<uuid::Uuid>() {
            Ok(uuid) => ChannelId::from(uuid),
            Err(e) => {
                tracing::error!("Invalid channel ID {}: {}", channel_id, e);
                return;
            }
        };
        
        match event {
            Event::MemberAdded(member) => {
                let user_id = match member.user_id.parse::<uuid::Uuid>() {
                    Ok(uuid) => UserId::from(uuid),
                    Err(e) => {
                        tracing::error!("Invalid user ID {}: {}", member.user_id, e);
                        return;
                    }
                };
                
                // The server's join time is the join's identity: the live
                // event and a later roster sync key the notice on it, so the
                // same join is filed once whichever path sees it first.
                let joined_at = (member.joined_at > 0)
                    .then(|| chrono::DateTime::from_timestamp_nanos(member.joined_at as i64))
                    .unwrap_or_else(Utc::now);
                let membership = Membership {
                    channel_id,
                    user_id,
                    role: MemberRole::Member,
                    notification_preference: NotificationPreference::All,
                    joined_at,
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                };
                let key = if member.joined_at > 0 { membership_ts(&membership) } else { timestamp };
                self.publish_member_joined(&membership, key);
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
            Event::MemberUpdated(proto_member) => {
                use crate::services::channel::converters::{mute_expiry, proto_role_to_domain};

                let user_id = match proto_member.user_id.parse::<uuid::Uuid>() {
                    Ok(uuid) => UserId::from(uuid),
                    Err(e) => {
                        tracing::error!("Invalid user ID {}: {}", proto_member.user_id, e);
                        return;
                    }
                };
                let new_role = proto_role_to_domain(proto_member.role);
                // The server sends the whole member on a role change and on a
                // mute, unmute or mute expiry, so take its moderator mute too.
                let (is_muted, muted_until) = (proto_member.muted, mute_expiry(&proto_member));

                // Update the cached membership role. If we don't have the target
                // member cached (e.g. an admin changed a THIRD user's role and we
                // never fetched that membership), insert a minimal record so the
                // member list reflects the new role.
                match self.channel_repo.get_membership(&channel_id, &user_id) {
                    Ok(Some(mut membership)) => {
                        membership.role = new_role;
                        membership.is_muted = is_muted;
                        membership.muted_until = muted_until;
                        if let Err(e) = self.channel_repo.update_membership(&membership) {
                            tracing::warn!("Failed to persist member role change locally: {}", e);
                            return;
                        }
                    }
                    Ok(None) => {
                        let membership = Membership {
                            channel_id,
                            user_id,
                            role: new_role,
                            notification_preference: NotificationPreference::All,
                            joined_at: Utc::now(),
                            last_read_message_id: None,
                            is_muted,
                            muted_until,
                        };
                        if let Err(e) = self.channel_repo.insert_membership(&membership) {
                            tracing::warn!("Failed to insert member for role change locally: {}", e);
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read membership for role change: {}", e);
                        return;
                    }
                }

                let event = DomainEvent::ChannelMemberUpdated {
                    channel_id,
                    user_id,
                    role: new_role,
                };
                if let Err(e) = self.event_bus.domain_sender().send(event) {
                    tracing::warn!("Failed to publish ChannelMemberUpdated event: {}", e);
                }
                tracing::info!(
                    "Channel member role changed: channel={}, user={}, role={:?}",
                    channel_id, user_id, new_role
                );
            }
            Event::MemberRemoved(removed) => {
                let user_id = match removed.user_id.parse::<uuid::Uuid>() {
                    Ok(uuid) => UserId::from(uuid),
                    Err(e) => {
                        tracing::error!("Invalid user ID {}: {}", removed.user_id, e);
                        return;
                    }
                };
                let reason = member_removal_reason(&removed);
                self.publish_member_removed(&channel_id, &user_id, reason, timestamp);
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
                if let Err(e) = self.channel_repo.delete_channel_cascade(&target) {
                    tracing::warn!("Failed to delete channel locally: {}", e);
                }
                let _ = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::ChannelDeleted { channel_id: target });
                tracing::info!("Channel deleted: channel={}", target);
            }

            Event::ChannelArchived(id) => self.apply_archive_state(&id, channel_id, true),
            Event::ChannelUnarchived(id) => self.apply_archive_state(&id, channel_id, false),

            Event::ModerationAction(action) => {
                let (Ok(user_uuid), Ok(mod_uuid)) = (
                    action.user_id.parse::<uuid::Uuid>(),
                    action.moderator_id.parse::<uuid::Uuid>(),
                ) else {
                    tracing::error!("Moderation event with unparseable user ids");
                    return;
                };
                let user_id = UserId::from(user_uuid);
                let moderator_id = UserId::from(mod_uuid);
                let action_name = proto_moderation_action_name(action.action);
                let is_self = self
                    .settings_repo
                    .current_user_id()
                    .map(|me| me == user_id)
                    .unwrap_or(false);

                // A kick or ban that targets US means we are out of the
                // channel: drop the local membership so the roster and the
                // send path stop pretending otherwise.
                if is_self && matches!(action_name, "KICK" | "BAN") {
                    if let Err(e) = self.channel_repo.delete_membership(&channel_id, &user_id) {
                        tracing::warn!("Failed to remove own membership after {}: {}", action_name, e);
                    }
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
        }
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

    /// A roster sync found `membership` on the server but not locally: the
    /// `MemberAdded` event was missed (app backgrounded, stream down). Files
    /// the notice the event would have. Keyed on the server's `joined_at`, so
    /// every sync that sees this join files it once.
    pub fn record_synced_join(&self, membership: &Membership) {
        self.publish_member_joined(membership, membership_ts(membership));
    }

    /// A roster sync found `membership` gone from the server: the
    /// `MemberRemoved` event was missed. The roster carries no reason, so it
    /// reads as a leave. Keyed on the membership that ended.
    pub fn record_synced_departure(&self, membership: &Membership) {
        self.publish_member_removed(
            &membership.channel_id,
            &membership.user_id,
            MemberRemoval::Left,
            membership_ts(membership),
        );
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
            MemberRemoval::Kicked(by) | MemberRemoval::Banned(by) if is_self => {
                (SystemMessage::SelfRemoved { by }, by)
            }
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
    // ponytail: time window, because the RPC and the broadcast carry no shared
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
        }
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

/// Why a member dropped off the roster, from the server's `MemberRemoved`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRemoval {
    Left,
    Kicked(UserId),
    Banned(UserId),
}

/// A removal whose moderator id is missing or unparseable is reported as a
/// leave: the roster change is still applied, only the wording is generic.
fn member_removal_reason(removed: &crate::proto::chat_v1::MemberRemoved) -> MemberRemoval {
    use crate::proto::chat_v1::member_removed::Reason;
    let by = removed.moderator_id.parse::<uuid::Uuid>().ok().map(UserId::from);
    match (Reason::try_from(removed.reason), by) {
        (Ok(Reason::Kicked), Some(by)) => MemberRemoval::Kicked(by),
        (Ok(Reason::Banned), Some(by)) => MemberRemoval::Banned(by),
        _ => MemberRemoval::Left,
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

    #[test]
    fn a_removal_keeps_its_reason_and_moderator() {
        use crate::proto::chat_v1::{member_removed::Reason, MemberRemoved};
        let by = UserId::new();
        let ev = |reason: Reason, moderator_id: String| MemberRemoved {
            user_id: UserId::new().to_string(),
            reason: reason as i32,
            moderator_id,
        };
        assert_eq!(member_removal_reason(&ev(Reason::Banned, by.to_string())), MemberRemoval::Banned(by));
        assert_eq!(member_removal_reason(&ev(Reason::Kicked, by.to_string())), MemberRemoval::Kicked(by));
        assert_eq!(member_removal_reason(&ev(Reason::Left, String::new())), MemberRemoval::Left);
        // No moderator to name: fall back to the neutral wording.
        assert_eq!(member_removal_reason(&ev(Reason::Banned, String::new())), MemberRemoval::Left);
    }

    fn setup() -> (Arc<ChannelEventService>, EventBus, Arc<ChannelRepository>) {
        // SQLCipher needs a real path (see call_repo tests).
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("channel_events_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        Box::leak(Box::new(temp_dir));

        let channel_repo = Arc::new(ChannelRepository::new(db.clone()));
        let event_bus = EventBus::new();
        // Never reachable: the rollback fetch is expected to fail, which is
        // the "server no longer has this channel" path.
        let grpc_pool = Arc::new(
            crate::network::grpc::client_pool::GrpcClientPool::new("http://127.0.0.1:1").unwrap(),
        );

        let service = Arc::new(ChannelEventService::new(
            Arc::new(MessageRepository::new(db.clone())),
            channel_repo.clone(),
            Arc::new(UserRepository::new(db.clone())),
            grpc_pool,
            event_bus.clone(),
            Arc::new(crate::storage::repositories::settings_repo::SettingsRepository::new(db)),
        ));

        (service, event_bus, channel_repo)
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
                0,
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

    /// A deleted channel must actually disappear: local rows gone and the UI
    /// told. This arm was silently discarded too.
    #[tokio::test]
    async fn channel_deleted_removes_local_channel_and_notifies() {
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();

        let channel = crate::domain::channel::Channel::new(
            ChannelId::from(uuid::Uuid::new_v4()),
            "doomed".to_string(),
            crate::domain::channel::ChannelType::Public,
            UserId::from(uuid::Uuid::new_v4()),
        );
        let channel_id = channel.id;
        channel_repo.upsert(&channel).unwrap();

        service
            .handle_stream_event(
                channel_event::Event::ChannelDeleted(channel_id.to_string()),
                &channel_id.to_string(),
                0,
            )
            .await;

        assert!(
            channel_repo.get(&channel_id).unwrap().is_none(),
            "deleted channel still present locally"
        );
        match rx.try_recv() {
            Ok(DomainEvent::ChannelDeleted { channel_id: got }) => assert_eq!(got, channel_id),
            other => panic!("ChannelDeleted did not reach the domain bus: {:?}", other),
        }
    }

    /// F77: the server delivers one membership event on two paths (channel
    /// stream + per-user stream). Each must file exactly one notice and tell
    /// the UI once; a later event for the same user is a new notice.
    #[tokio::test]
    async fn a_membership_event_delivered_twice_files_one_notice() {
        use crate::proto::chat_v1::{channel::ChannelMember as ProtoMember, member_removed::Reason, MemberRemoved};
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();

        let channel = crate::domain::channel::Channel::new(
            ChannelId::from(uuid::Uuid::new_v4()),
            "invtest".to_string(),
            crate::domain::channel::ChannelType::Public,
            UserId::from(uuid::Uuid::new_v4()),
        );
        let channel_id = channel.id;
        channel_repo.upsert(&channel).unwrap();
        let joiner = UserId::from(uuid::Uuid::new_v4());
        let count = || service.message_repo.count(&channel_id, &ConversationType::Channel).unwrap();
        let added = || channel_event::Event::MemberAdded(ProtoMember {
            user_id: joiner.to_string(),
            ..Default::default()
        });
        let left = || channel_event::Event::MemberRemoved(MemberRemoved {
            user_id: joiner.to_string(),
            reason: Reason::Left as i32,
            moderator_id: String::new(),
        });

        for _ in 0..2 {
            service.handle_stream_event(added(), &channel_id.to_string(), 1_000).await;
        }
        assert_eq!(count(), 1, "one join filed two notices");
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberJoined { .. })));
        assert!(rx.try_recv().is_err(), "the duplicate reached the UI");

        for _ in 0..2 {
            service.handle_stream_event(left(), &channel_id.to_string(), 2_000).await;
        }
        assert_eq!(count(), 2, "one leave filed two notices");
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberLeft { .. })));
        assert!(rx.try_recv().is_err(), "the duplicate reached the UI");

        // Re-joining later is a new event and a new notice.
        service.handle_stream_event(added(), &channel_id.to_string(), 3_000).await;
        assert_eq!(count(), 3);
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

    /// F79: our own leave arrives from the leave RPC and from the channel
    /// stream's broadcast, with unrelated timestamps, in either order. One
    /// "You left" and one UI event; leaving again after a re-join is new.
    #[tokio::test]
    async fn own_leave_seen_on_both_paths_files_one_notice() {
        use crate::proto::chat_v1::{channel::ChannelMember as ProtoMember, member_removed::Reason, MemberRemoved};
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();
        let me = as_me(&service);
        let channel_id = private_channel(&channel_repo);
        let count = || service.message_repo.count(&channel_id, &ConversationType::Channel).unwrap();
        let broadcast = || channel_event::Event::MemberRemoved(MemberRemoved {
            user_id: me.to_string(),
            reason: Reason::Left as i32,
            moderator_id: String::new(),
        });

        service.handle_stream_event(broadcast(), &channel_id.to_string(), 1_000).await;
        service.record_own_leave(&channel_id);
        assert_eq!(count(), 1, "broadcast then RPC filed two notices");
        assert!(matches!(rx.try_recv(), Ok(DomainEvent::ChannelMemberLeft { .. })));
        assert!(rx.try_recv().is_err(), "the duplicate reached the UI");

        let rejoin = channel_event::Event::MemberAdded(ProtoMember { user_id: me.to_string(), ..Default::default() });
        service.handle_stream_event(rejoin, &channel_id.to_string(), 2_000).await;
        service.record_own_leave(&channel_id);
        service.handle_stream_event(broadcast(), &channel_id.to_string(), 3_000).await;
        assert_eq!(count(), 3, "join + second leave, the RPC-first order deduped");
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

        service.record_synced_departure(&member);
        service.record_synced_departure(&member);
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

    /// A ban on the local user drops our own membership and surfaces to the UI.
    #[tokio::test]
    async fn moderation_ban_on_self_is_applied_and_announced() {
        let (service, bus, channel_repo) = setup();
        let mut rx = bus.domain_sender().subscribe();

        let me = UserId::from(uuid::Uuid::new_v4());
        // Pretend we are logged in as `me`.
        let settings = &service.settings_repo;
        settings
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();

        let channel = crate::domain::channel::Channel::new(
            ChannelId::from(uuid::Uuid::new_v4()),
            "banned-from".to_string(),
            crate::domain::channel::ChannelType::Public,
            me,
        );
        let channel_id = channel.id;
        channel_repo.upsert(&channel).unwrap();
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
                    action: crate::proto::chat_v1::channel_moderation::Action::Ban as i32,
                    duration: None,
                    reason: "spam".to_string(),
                }),
                &channel_id.to_string(),
                0,
            )
            .await;

        assert!(
            channel_repo.get_membership(&channel_id, &me).unwrap().is_none(),
            "own membership survived a ban"
        );
        match rx.try_recv() {
            Ok(DomainEvent::ChannelModerationAction { action, is_self, reason, .. }) => {
                assert_eq!(action, "BAN");
                assert!(is_self);
                assert_eq!(reason.as_deref(), Some("spam"));
            }
            other => panic!("ModerationAction did not reach the domain bus: {:?}", other),
        }
    }
}
