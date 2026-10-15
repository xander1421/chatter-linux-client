//! Account notifications and channel-owned server records. Never changes local
//! encrypted call history or treats server call events as signaling authority.
use crate::error::NetworkError;
use crate::network::grpc::GrpcClientPool;

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
            let notifications = response.notifications.into_iter().map(|n| {
                let kind = match crate::proto::chat_v1::NotificationType::try_from(n.r#type) {
                    Ok(crate::proto::chat_v1::NotificationType::FriendRequest) => "friend-request",
                    Ok(crate::proto::chat_v1::NotificationType::FriendAccepted) => "friend-accepted",
                    Ok(crate::proto::chat_v1::NotificationType::ChannelInvite) => "channel-invite",
                    Ok(crate::proto::chat_v1::NotificationType::Message) => "message",
                    Ok(crate::proto::chat_v1::NotificationType::Mention) => "mention",
                    Ok(crate::proto::chat_v1::NotificationType::Reply) => "reply",
                    Ok(crate::proto::chat_v1::NotificationType::Reaction) => "reaction",
                    _ => "other",
                };
                NotificationRow { id: n.id, title: n.title, body: n.body,
                    actor_id: n.actor_id, avatar: None, channel_id: n.channel_id,
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
