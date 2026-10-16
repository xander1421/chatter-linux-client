//! Account notifications and channel-owned server records. Never changes local
//! encrypted call history or treats server call events as signaling authority.
use crate::error::NetworkError;
use crate::network::grpc::GrpcClientPool;
use std::collections::HashSet;

#[derive(Clone, Debug, Default)]
pub struct RecordRow {
    pub id: String,
    pub title: String,
    pub body: String,
    pub primary: String,
    pub secondary: String,
}

#[derive(Default)]
pub struct RecordPage {
    pub rows: Vec<RecordRow>,
    pub notifications: Vec<NotificationRow>,
    pub next: String,
}

#[derive(Clone, Debug, Default)]
pub struct NotificationRow {
    pub id: String,
    pub title: String,
    pub body: String,
    pub actor_id: String,
    pub avatar: Option<Vec<u8>>,
    pub device_id: u32,
    pub channel_id: String,
    pub kind: String,
    pub when: String,
    pub read: bool,
    pub status: String,
}

fn relative_time(timestamp: u64) -> String {
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let seconds = now.saturating_sub(timestamp / 1_000_000_000);
    match seconds {
        0..=59 => "Now".into(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86399 => format!("{}h", seconds / 3600),
        86400..=604799 => format!("{}d", seconds / 86400),
        _ => chrono::DateTime::from_timestamp((timestamp / 1_000_000_000) as i64, 0)
            .map(|t| t.format("%d %b %Y").to_string())
            .unwrap_or_default(),
    }
}

fn offset(cursor: &str) -> Result<i32, NetworkError> {
    if cursor.is_empty() {
        return Ok(0);
    }
    cursor
        .parse::<i32>()
        .ok()
        .filter(|n| *n >= 0)
        .ok_or_else(|| NetworkError::InvalidRequest("Invalid page cursor".into()))
}

/// The notification has no friend-request ID, so its current relationship
/// cannot establish the outcome of an older request from the same person.
pub fn collapse_friend_requests(rows: &mut Vec<NotificationRow>) {
    let mut seen = HashSet::new();
    rows.retain(|row| row.kind != "friend-request"
        || row.actor_id.is_empty()
        || seen.insert(row.actor_id.clone()));
}

fn device_linked_details(body: &str, metadata: &str) -> (String, u32) {
    let meta = serde_json::from_str::<serde_json::Value>(metadata).ok();
    let device_id = meta.as_ref().and_then(|m| m.get("linked_device_id")
        .or_else(|| m.get("device_id"))).and_then(|v| v.as_u64())
        .and_then(|id| u32::try_from(id).ok()).unwrap_or_default();
    let label = meta.as_ref().and_then(|m| {
        m.get("device_name").or_else(|| m.get("device_info"))
            .and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
    });
    let body = match label {
        Some(label) => format!("{label} was linked to your account."),
        None if body.starts_with("A Android ") => body.replacen("A Android ", "Android ", 1),
        None => body.to_owned(),
    };
    (body, device_id)
}

fn notification_actor(actor_id: String, metadata: &str) -> String {
    if !actor_id.is_empty() { return actor_id; }
    serde_json::from_str::<serde_json::Value>(metadata).ok()
        .and_then(|meta| ["actor_id", "friend_id", "sender_id", "requester_id"].iter()
            .find_map(|key| meta.get(key).and_then(|v| v.as_str()).map(str::to_owned)))
        .unwrap_or_default()
}

pub async fn page(
    pool: &GrpcClientPool,
    kind: &str,
    channel: &str,
    cursor: &str,
    user: &str,
) -> Result<RecordPage, NetworkError> {
    match kind {
        "notifications" => {
            let start = offset(cursor)?;
            let response = pool
                .notification_client()
                .list_notifications(false, vec![], 50, start)
                .await?;
            let end = start.saturating_add(response.notifications.len() as i32);
            let mut items = response.notifications;
            items.sort_by_key(|n| std::cmp::Reverse(n.created_at));
            let notifications = items.into_iter().map(|n| {
                let kind = match crate::proto::chat_v1::NotificationType::try_from(n.r#type) {
                    Ok(crate::proto::chat_v1::NotificationType::FriendRequest) => "friend-request",
                    Ok(crate::proto::chat_v1::NotificationType::FriendAccepted) => "friend-accepted",
                    Ok(crate::proto::chat_v1::NotificationType::ChannelInvite) => "channel-invite",
                    Ok(crate::proto::chat_v1::NotificationType::Message) => "message",
                    Ok(crate::proto::chat_v1::NotificationType::Mention) => "mention",
                    Ok(crate::proto::chat_v1::NotificationType::Reply) => "reply",
                    Ok(crate::proto::chat_v1::NotificationType::Reaction) => "reaction",
                    Ok(crate::proto::chat_v1::NotificationType::DeviceLinked) => "device-linked",
                    _ => "other",
                };
                let (body, device_id) = if kind == "device-linked" {
                    device_linked_details(&n.body, &n.metadata)
                } else { (n.body, 0) };
                let (title, body) = if kind == "friend-accepted" && n.actor_id.is_empty() {
                    if let Some((name, message)) = body.split_once(" · ") {
                        if message == "Your friend request was accepted" {
                            (name.to_owned(), message.to_owned())
                        } else { (n.title, body) }
                    } else { (n.title, body) }
                } else { (n.title, body) };
                let actor_id = if kind == "friend-request" || kind == "friend-accepted" {
                    notification_actor(n.actor_id, &n.metadata)
                } else { n.actor_id };
                NotificationRow { id: n.id, title, body,
                    actor_id, avatar: None,
                    device_id, channel_id: n.channel_id,
                    kind: kind.into(), when: relative_time(n.created_at), read: n.read,
                    status: String::new() }
            }).collect();
            Ok(RecordPage {
                next: if end < response.total_count && end > start {
                    end.to_string()
                } else {
                    String::new()
                },
                rows: Vec::new(), notifications,
            })
        }
        "integrations" => {
            let mut client = pool.integration_client();
            let installed = client.get_channel_integrations(channel).await?.integrations;
            let response = client
                .list_integrations(None, Some(cursor), Some(50))
                .await?;
            let installed_ids: std::collections::HashSet<_> =
                installed.iter().map(|i| i.id.clone()).collect();
            let mut rows = Vec::new();
            if cursor.is_empty() {
                rows.extend(installed.into_iter().map(|i| {
                    RecordRow {
                        secondary: if i.installed_by == user {
                            "Remove".into()
                        } else {
                            String::new()
                        },
                        id: i.id,
                        title: i.name,
                        body: if i.installed_by == user {
                            "Installed in this channel"
                        } else {
                            "Installed · managed by another owner"
                        }
                        .into(),
                        primary: String::new(),
                    }
                }));
            }
            rows.extend(
                response
                    .integrations
                    .into_iter()
                    .filter(|i| !installed_ids.contains(&i.id))
                    .map(|i| RecordRow {
                        id: i.id,
                        title: i.name,
                        body: "Available from your integrations".into(),
                        primary: "Install".into(),
                        secondary: String::new(),
                    }),
            );
            Ok(RecordPage {
                rows,
                next: response.next_page_token,
                ..Default::default()
            })
        }
        "calls" => {
            let start = offset(cursor)?;
            let (calls, _, more) = pool
                .call_client()
                .list_channel_calls(channel, None, 50, start)
                .await?;
            let next = start.saturating_add(calls.len() as i32);
            Ok(RecordPage {
                next: if more && next > start {
                    next.to_string()
                } else {
                    String::new()
                },
                rows: calls
                    .into_iter()
                    .map(|c| {
                        let when = chrono::DateTime::from_timestamp(
                            (c.created_at / 1_000_000_000) as i64,
                            (c.created_at % 1_000_000_000) as u32,
                        )
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_default();
                        let status = match crate::proto::chat_v1::CallStatus::try_from(c.status) {
                            Ok(crate::proto::chat_v1::CallStatus::Active) => "Active",
                            Ok(crate::proto::chat_v1::CallStatus::Ended) => "Ended",
                            _ => "Unknown status",
                        };
                        RecordRow {
                            id: c.id,
                            title: c.title,
                            body: format!("{status} · {when}"),
                            ..Default::default()
                        }
                    })
                    .collect(),
                ..Default::default()
            })
        }
        _ => Err(NetworkError::InvalidRequest(
            "Unknown server record screen".into(),
        )),
    }
}

pub async fn action(
    pool: &GrpcClientPool,
    kind: &str,
    channel: &str,
    id: &str,
    action: &str,
) -> Result<(), NetworkError> {
    match (kind, action) {
        ("notifications", "Mark read") => {
            pool.notification_client()
                .mark_as_read(vec![id.into()])
                .await?;
        }
        ("notifications", "Delete") => {
            pool.notification_client().delete_notification(id).await?;
        }
        ("notifications", "Mark all read") => {
            pool.notification_client().mark_all_as_read(vec![]).await?;
        }
        ("integrations", "Install") => {
            pool.integration_client()
                .add_integration_to_channel(id, channel)
                .await?;
        }
        ("integrations", "Remove") => {
            pool.integration_client()
                .remove_integration_from_channel(id, channel)
                .await?;
        }
        _ => return Err(NetworkError::InvalidRequest("Unknown record action".into())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursors_reject_negative_and_overflowing_offsets() {
        assert_eq!(offset("").unwrap(), 0);
        assert_eq!(offset("50").unwrap(), 50);
        for invalid in ["-1", "2147483648", "garbage"] {
            assert!(offset(invalid).is_err());
        }
    }
}
