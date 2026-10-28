use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::domain::mention::{MentionExtractor, MentionType};
use crate::domain::message::{ConversationType, Message};
use crate::events::DomainEvent;
use crate::services::activity_service::{ActivityItem, ActivityTarget, NewActivity};
use crate::services::notification_service::NotificationDetailLevel;

use super::ChatterClient;

/// Device facts for the Activity Center. Never exposes an endpoint path or push topic.
pub struct ActivityStatus {
    pub push: &'static str,
    pub last_connected: Option<chrono::DateTime<chrono::Utc>>,
    pub server_host: String,
    pub tor_bootstrap_percent: Option<u8>,
}

impl ChatterClient {
    pub fn activity_status(&self) -> ActivityStatus {
        let push = if self.push_url.lock().is_none() {
            "Off"
        } else if self.anon_mode_enabled() {
            "Paused in Shield"
        } else if self.push_shutdown.lock().is_some() {
            "Registered"
        } else {
            "Configured, not registered"
        };
        let server_host = url::Url::parse(&self.endpoint())
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "Unknown".into());
        ActivityStatus {
            push,
            last_connected: self.offline_snapshot().last_connected,
            server_host,
            tor_bootstrap_percent: {
                #[cfg(feature = "onion")]
                { crate::network::grpc::onion_transport::bootstrap_percent() }
                #[cfg(not(feature = "onion"))]
                { None }
            },
        }
    }

    pub fn activity_items(&self) -> Result<Vec<ActivityItem>, crate::error::StorageError> {
        let Some(owner) = self.auth_snapshot().current_user_id else { return Ok(Vec::new()) };
        let show_preview = self.notification_detail_level() == NotificationDetailLevel::NameAndMessage;
        self.activity.list(owner, show_preview, |user_ids| self.users().get_display_names(user_ids))
    }

    pub fn activity_unread_count(&self) -> Result<u32, crate::error::StorageError> {
        match self.auth_snapshot().current_user_id {
            Some(owner) => self.activity.unread_count(owner),
            None => Ok(0),
        }
    }

    pub fn activity_target(&self, id: &str) -> Option<ActivityTarget> {
        self.auth_snapshot().current_user_id
            .and_then(|owner| self.activity.target(owner, id).ok().flatten())
    }

    pub fn activity_mark_read(&self, id: &str) {
        if let Some(owner) = self.auth_snapshot().current_user_id {
            if let Err(e) = self.activity.mark_read(owner, id) {
                tracing::warn!(error = %e, "Could not mark activity read");
            }
        }
    }

    /// The conversation was read: its mention/reply/reaction rows are too.
    pub(crate) fn activity_mark_conversation_read(&self, conversation_id: &str) {
        if let Some(owner) = self.auth_snapshot().current_user_id {
            if let Err(e) = self.activity.mark_conversation_read(owner, conversation_id) {
                tracing::warn!(error = %e, "Could not mark activity read");
            }
        }
    }

    pub fn activity_mark_all_read(&self) {
        if let Some(owner) = self.auth_snapshot().current_user_id {
            if let Err(e) = self.activity.mark_all_read(owner) {
                tracing::warn!(error = %e, "Could not mark activity read");
            }
        }
    }

    pub fn subscribe_activity(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.activity.subscribe()
    }

    pub fn start_activity_listener(self: &Arc<Self>) {
        if self.activity_listener_started.swap(true, Ordering::AcqRel) { return; }
        if let Some(owner) = self.auth_snapshot().current_user_id {
            if let Err(e) = self.activity.sync_missed_calls(owner) {
                tracing::warn!(error = %e, "Could not record missed calls");
            }
            let state = self.offline_snapshot().connection_state;
            if self.server_unreachable_for_state(state) {
                if let Err(e) = self.activity.server_reachability(owner, true) {
                    tracing::warn!(error = %e, "Could not record server status");
                }
            }
            #[cfg(feature = "mesh")]
            self.sync_mesh_invitations(owner);
        }
        let mut events = self.services.event_bus.subscribe_domain();
        let mut offline = self.subscribe_offline_snapshots();
        let mut auth = self.subscribe_auth_snapshots();
        let client = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => client.capture_activity_event(event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    state = offline.recv() => match state {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Some(owner) = client.auth_snapshot().current_user_id {
                                let state = client.offline_snapshot().connection_state;
                                let unreachable = client.server_unreachable_for_state(state);
                                if unreachable || matches!(state, super::ConnectionState::Connected) {
                                    if let Err(e) = client.activity.server_reachability(owner, unreachable) {
                                        tracing::warn!(error = %e, "Could not record server status");
                                    }
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    state = auth.recv() => match state {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Some(owner) = client.auth_snapshot().current_user_id {
                                if let Err(e) = client.activity.sync_missed_calls(owner) {
                                    tracing::warn!(error = %e, "Could not record missed calls");
                                }
                                let state = client.offline_snapshot().connection_state;
                                if client.server_unreachable_for_state(state) {
                                    if let Err(e) = client.activity.server_reachability(owner, true) {
                                        tracing::warn!(error = %e, "Could not record server status");
                                    }
                                }
                                #[cfg(feature = "mesh")]
                                client.sync_mesh_invitations(owner);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        });
        self.tasks.lock().push(handle);

        #[cfg(feature = "mesh")]
        {
            let mut mesh = self.mesh_subscribe();
            let client = self.clone();
            let handle = tokio::spawn(async move {
                loop {
                    match mesh.recv().await {
                        Ok(crate::services::mesh::MeshEvent::PrivateChannelInvitation { id, name, nickname, .. }) => {
                            let Some(owner) = client.auth_snapshot().current_user_id else { continue };
                            client.record_activity(owner, NewActivity {
                                id: &format!("mesh-invite:{id}"), kind: "mesh-invite",
                                actor_id: None, actor_name: &nickname, detail: &name, preview: "", conversation_id: None, conversation_type: None,
                                message_id: None, created_at_ms: chrono::Utc::now().timestamp_millis(),
                            });
                        }
                        Ok(_) => {},
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Some(owner) = client.auth_snapshot().current_user_id {
                                client.sync_mesh_invitations(owner);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            self.tasks.lock().push(handle);
        }
    }

    fn record_activity(&self, owner: crate::domain::UserId, item: NewActivity<'_>) {
        if let Err(e) = self.activity.record(owner, item) {
            tracing::warn!(error = %e, "Could not record activity");
        }
    }

    #[cfg(feature = "mesh")]
    fn sync_mesh_invitations(&self, owner: crate::domain::UserId) {
        for (id, name, nickname) in self.mesh_pending_private_invitations() {
            self.record_activity(owner, NewActivity {
                id: &format!("mesh-invite:{id}"), kind: "mesh-invite",
                actor_id: None, actor_name: &nickname, detail: &name, preview: "", conversation_id: None, conversation_type: None,
                message_id: None, created_at_ms: chrono::Utc::now().timestamp_millis(),
            });
        }
    }

    fn capture_activity_event(&self, event: DomainEvent) {
        let Some(owner) = self.auth_snapshot().current_user_id else { return };
        let now = chrono::Utc::now().timestamp_millis();
        match event {
            DomainEvent::MessageReceived { message } => self.capture_message(owner, &message),
            DomainEvent::ReactionAdded { scope, sender_id, sent_at_ns, reactor_id, emoji, timestamp } if sender_id == owner && reactor_id != owner => {
                let target_message = self.get_message_by_sender_timestamp(&sender_id, sent_at_ns)
                    .filter(|message| message.conversation_id == scope.id && message.conversation_type == scope.kind)
                    .map(|message| message.id.to_string());
                self.record_activity(owner, NewActivity {
                    id: &format!("reaction:{}:{sent_at_ns}:{reactor_id}:{emoji}", scope.id),
                    kind: "reaction", actor_id: Some(reactor_id), actor_name: "", detail: "", preview: &emoji,
                    conversation_id: Some(&scope.id.to_string()),
                    conversation_type: Some(scope.kind.as_str()), message_id: target_message.as_deref(),
                    created_at_ms: if timestamp == 0 { now } else { (timestamp.min(now as u64)) as i64 },
                });
            }
            // A withdrawn reaction takes its row with it, as in Telegram.
            DomainEvent::ReactionRemoved { scope, sender_id, sent_at_ns, reactor_id, emoji, .. } if sender_id == owner && reactor_id != owner => {
                let id = format!("reaction:{}:{sent_at_ns}:{reactor_id}:{emoji}", scope.id);
                if let Err(e) = self.activity.forget(owner, &id) {
                    tracing::warn!(error = %e, "Could not drop a withdrawn reaction's Activity row");
                }
            }
            DomainEvent::FriendRequestsUpdated { received, .. } => {
                for request in received {
                    let actor_id = request.user_id.parse().ok();
                    self.record_activity(owner, NewActivity {
                        id: &format!("contact-request:{}:{}", request.user_id, request.created_at_ns),
                        kind: "contact-request", actor_id, actor_name: &request.display_name, detail: "", preview: "",
                        conversation_id: None, conversation_type: None, message_id: None,
                        created_at_ms: if request.created_at_ns == 0 { now } else { (request.created_at_ns / 1_000_000) as i64 },
                    });
                }
            }
            DomainEvent::ChannelInviteReceived { channel_id, inviter_id, message, expires_at, channel_name } => {
                let channel = self
                    .get_channel_local(&channel_id)
                    .map(|c| c.name)
                    .or_else(|| (!channel_name.is_empty()).then_some(channel_name))
                    .unwrap_or_else(|| "a channel".to_owned());
                self.record_activity(owner, NewActivity {
                    id: &format!("invite:{channel_id}:{inviter_id}:{expires_at}"), kind: "invite",
                    actor_id: Some(inviter_id), actor_name: "", detail: &channel, preview: message.as_deref().unwrap_or(""),
                    conversation_id: Some(&channel_id.to_string()), conversation_type: Some("channel"),
                    message_id: None, created_at_ms: now,
                });
            }
            DomainEvent::AnonGroupInviteReceived { group_id, group_name, inviter_id, inviter_display_name, .. } => {
                self.record_activity(owner, NewActivity {
                    id: &format!("invite:anon:{group_id}"), kind: "invite",
                    actor_id: Some(inviter_id), actor_name: inviter_display_name.as_deref().unwrap_or(""), detail: &group_name,
                    preview: "", conversation_id: Some(&group_id.to_string()),
                    conversation_type: Some("anon_group"), message_id: None, created_at_ms: now,
                });
            }
            DomainEvent::DeviceLinked { device_id, device_name, .. } => {
                self.record_activity(owner, NewActivity {
                    id: &format!("device-linked:{device_id}"), kind: "device-linked",
                    actor_id: None, actor_name: "", detail: device_name.as_deref().unwrap_or(""),
                    preview: "", conversation_id: None, conversation_type: None, message_id: None,
                    created_at_ms: now,
                });
            }
            DomainEvent::CallHistoryChanged => {
                if let Err(e) = self.activity.sync_missed_calls(owner) {
                    tracing::warn!(error = %e, "Could not record missed calls");
                }
            }
            _ => {}
        }
    }

    fn capture_message(&self, owner: crate::domain::UserId, message: &Message) {
        if message.sender_id.is_none() || message.sender_id == Some(owner) { return; }
        let sender = message.sender_id.unwrap();
        let text = message.as_text().unwrap_or("");
        let mentioned = self.message_mentions_user(message, &owner);
        // A thread I started or have replied in, as Slack's thread followers.
        let in_my_thread = message.thread_id.filter(|id| {
            self.get_message_local(id).is_some_and(|parent| parent.sender_id == Some(owner))
                || self.messages().has_thread_reply_from(id, &owner).unwrap_or(false)
        });
        let reply_to_me = message.reply_to_author_id == Some(owner)
            || message.reply_to.and_then(|id| self.get_message_local(&id)).is_some_and(|parent| parent.sender_id == Some(owner))
            || in_my_thread.is_some();
        // A thread I only replied in is not a reply to me: "replied in a thread".
        let followed_only = in_my_thread.is_some()
            && message.reply_to_author_id != Some(owner)
            && message.thread_id.and_then(|id| self.get_message_local(&id))
                .is_some_and(|root| root.sender_id != Some(owner));
        let kind = if mentioned { "mention" }
            else if followed_only { "thread" }
            else if reply_to_me { "reply" }
            else { return };
        // Replies in a thread of mine share one row, as Slack's "replied to
        // your thread": 55 replies were 55 identical rows.
        let id = match in_my_thread {
            Some(root) if kind != "mention" => format!("{}{root}", crate::services::activity_service::THREAD_ACTIVITY_PREFIX),
            _ => format!("{kind}:{}", message.id),
        };
        let place = if message.conversation_type == ConversationType::Channel {
            self.get_channel_local(&message.conversation_id).map(|c| format!(" in #{}", c.name)).unwrap_or_default()
        } else { String::new() };
        let preview = crate::domain::message::truncate_preview_text(text, 140);
        self.record_activity(owner, NewActivity {
            id: &id, kind,
            actor_id: Some(sender), actor_name: "", detail: &place, preview: &preview,
            conversation_id: Some(&message.conversation_id.to_string()),
            conversation_type: Some(message.conversation_type.as_str()),
            message_id: Some(&message.id.to_string()), created_at_ms: message.created_at.timestamp_millis(),
        });
    }

    pub(crate) fn message_mentions_user(&self, message: &Message, owner: &crate::domain::UserId) -> bool {
        let text = message.as_text().unwrap_or("");
        let username = self.get_user_local(owner).map(|u| u.username).unwrap_or_default();
        let display_name = self.get_profile_local(owner).map(|p| p.display_name).unwrap_or_default();
        MentionExtractor::extract_mentions(text).iter().any(|mention| {
            match mention.mention_type {
                MentionType::User => !username.is_empty() && mention.display_text[1..].eq_ignore_ascii_case(&username),
                MentionType::All | MentionType::Here => message.conversation_type == ConversationType::Channel,
                MentionType::Channel => false,
            }
        }) || named_mention(text, &display_name) || named_mention(text, &username)
    }
}

// Composer inserts the profile display name verbatim, including spaces, while
// MentionExtractor intentionally stops at the first non-identifier character.
fn named_mention(text: &str, name: &str) -> bool {
    if name.is_empty() { return false; }
    let text = text.to_lowercase();
    let needle = format!("@{}", name.to_lowercase());
    text.match_indices(&needle).any(|(start, _)| {
        text[start + needle.len()..].chars().next()
            .map_or(true, |next| !next.is_alphanumeric() && next != '_')
    })
}
