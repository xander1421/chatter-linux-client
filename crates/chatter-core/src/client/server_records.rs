use super::*;

impl ChatterClient {
    /// Server-backed account inbox / channel management presentation.
    pub async fn server_records_page(
        &self,
        kind: &str,
        channel: &str,
        cursor: &str,
    ) -> Result<crate::services::server_records::RecordPage, crate::error::NetworkError> {
        let user = self
            .session_current_user_id()
            .ok_or(crate::error::NetworkError::Unauthenticated)?
            .to_string();
        let mut page = crate::services::server_records::page(
            &self.services.grpc_pool,
            kind,
            channel,
            cursor,
            &user,
        )
        .await?;
        if kind == "notifications" {
            use crate::domain::ids::{ChannelId, UserId};
            use std::str::FromStr;
            let channels = self.list_channels().unwrap_or_default();
            let received = if page.notifications.iter().any(|n| n.kind == "friend-request") {
                self.services.grpc_pool.user_client()
                    .list_friend_requests(crate::proto::chat_v1::friend_requests_filter::Filter::Received, 100, 0)
                    .await.ok()
            } else { None };
            let invites = if page.notifications.iter().any(|n| n.kind == "channel-invite") {
                self.services.grpc_pool.channel_client().list_pending_invites().await.ok()
            } else { None };
            let sessions = if page.notifications.iter().any(|n| n.kind == "device-linked") {
                self.list_sessions().await.ok()
            } else { None };
            for row in &mut page.notifications {
                if row.kind == "device-linked" {
                    if let Some(session) = sessions.as_ref().and_then(|sessions| sessions.iter()
                        .find(|session| session.device_id == row.device_id && row.device_id != 0)) {
                        row.body = format!("{} was linked to your account.", session.device_info);
                    }
                }
                if row.kind == "channel-invite" {
                    if let Some(ref invites) = invites {
                        let pending = invites.invites.iter().find(|i| i.channel_id == row.channel_id);
                        if let Some(invite) = pending { row.body = invite.channel_name.clone(); }
                        row.status = (if pending.is_some() { "Pending" }
                            else if channels.iter().any(|c| c.id.to_string() == row.channel_id) { "Joined" }
                            else { "No longer pending" }).into();
                    }
                }
                if let Ok(channel_id) = ChannelId::from_str(&row.channel_id) {
                    if let Some(channel) = channels.iter().find(|c| c.id == channel_id) {
                        if row.kind == "channel-invite" { row.body = channel.name.clone(); }
                        if matches!(row.kind.as_str(), "message" | "mention" | "reply" | "thread" | "reaction") { row.title = channel.name.clone(); }
                    }
                }
                if let Ok(actor) = UserId::from_str(&row.actor_id) {
                    let request = received.as_ref().and_then(|r| r.requests.iter().find(|f| f.user_id == row.actor_id));
                    let name = request.map(|r| self.friend_request_peer_name(&actor, &r.username))
                        .unwrap_or_else(|| self.display_name(&actor));
                    row.avatar = self.user_avatar(&actor);
                    // The server title is generic for these events. Local names
                    // are preferred; a missing actor stays explicitly unnamed.
                    if row.kind == "friend-request" || row.kind == "friend-accepted" {
                        row.title = name;
                    } else if !name.starts_with("User ") {
                        row.body = crate::domain::message::format_preview_body(
                            &format!("{} · ", name), &row.body, None,
                        );
                    }
                    if row.kind == "friend-request" {
                        if let Some(ref received) = received {
                            row.status = match request.and_then(|r| crate::proto::chat_v1::FriendStatus::try_from(r.status).ok()) {
                                Some(crate::proto::chat_v1::FriendStatus::Pending) => "Pending",
                                Some(crate::proto::chat_v1::FriendStatus::Accepted) => "Accepted",
                                Some(crate::proto::chat_v1::FriendStatus::Declined) => "Declined",
                                Some(crate::proto::chat_v1::FriendStatus::Blocked) => "Blocked",
                                _ if self.is_friend(&actor) => "Accepted",
                                _ if received.total <= 100 => "No longer pending",
                                _ => "Status unavailable",
                            }.into();
                            row.body = match row.status.as_str() {
                                "Pending" => "Sent you a friend request",
                                "Accepted" => "You accepted this request",
                                "Declined" => "You declined this request",
                                "Blocked" => "This request is blocked",
                                "No longer pending" => "This request is no longer active",
                                _ => "Friend request",
                            }.into();
                        }
                    }
                }
                // A request or invite already answered (here or on another
                // device) has nothing left to act on: it no longer reads unread.
                if matches!(row.kind.as_str(), "friend-request" | "channel-invite")
                    && !row.status.is_empty() && row.status != "Pending"
                {
                    row.read = true;
                }
            }
            crate::services::server_records::collapse_friend_requests(&mut page.notifications);
        }
        Ok(page)
    }

    pub async fn server_records_action(
        &self,
        kind: &str,
        channel: &str,
        id: &str,
        action: &str,
    ) -> Result<(), crate::error::NetworkError> {
        crate::services::server_records::action(&self.services.grpc_pool, kind, channel, id, action)
            .await
    }

    // ── Escape hatches ──────────────────────────────────────────────────

    pub async fn server_record_detail(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<crate::services::server_records::RecordDetail, crate::error::NetworkError> {
        let mut detail =
            crate::services::server_records::detail(&self.services.grpc_pool, kind, id).await?;
        // Same device label as the inbox row, which resolves it from the session list.
        if detail.device_id != 0 {
            if let Some(session) = self.list_sessions().await.ok().and_then(|sessions| {
                sessions.into_iter().find(|session| session.device_id == detail.device_id)
            }) {
                detail.body = format!("{} was linked to your account.", session.device_info);
            }
        }
        Ok(detail)
    }

}
