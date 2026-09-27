//! Durable peer receipt application. Privacy and aggregation stay in core.
use super::{MessageError, MessageService};
use crate::domain::ids::{ChannelId, DeviceId, MessageId, UserId};
use crate::domain::message::ConversationType;
use crate::inbox::DomainEvent;
use crate::storage::repositories::message_receipt_repo::{DeviceReceipt, MessageReceiptRepository};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Preserve acknowledged progress when queued UI events arrive out of order.
pub fn advance_delivery_status<'a>(current: &'a str, incoming: &'a str) -> &'a str {
    if current == "read" || (current == "delivered" && incoming != "read") {
        current
    } else {
        incoming
    }
}

impl MessageService {
    /// Snapshot the actual peer devices targeted by a send, before transport.
    /// Exclude our linked devices: their read state travels via ReadSync.
    pub fn snapshot_receipt_targets(
        &self,
        message_id: MessageId,
        targets: &[(UserId, DeviceId)],
    ) -> Result<(), MessageError> {
        MessageReceiptRepository::new(self.database.clone()).snapshot_targets(
            message_id,
            self.current_user_id()?,
            targets,
        )?;
        Ok(())
    }

    /// Capture the actual DM fanout before any device can acknowledge it.
    pub(super) fn snapshot_dm_content_targets(
        &self,
        peer: UserId,
        content: &crate::proto::content_v1::Content,
        devices: &[u32],
    ) -> Result<(), MessageError> {
        let author = self.current_user_id()?;
        if peer == author {
            return Ok(());
        }
        let Some(crate::proto::content_v1::content::Content::DataMessage(message)) =
            &content.content
        else {
            return Ok(());
        };
        if !message.channel_id.is_empty()
            || (message.body.is_empty() && message.attachments.is_empty())
        {
            return Ok(());
        }
        let targets: Vec<_> = devices.iter().map(|id| (peer, DeviceId(*id))).collect();
        MessageReceiptRepository::new(self.database.clone()).snapshot_for_send(
            ChannelId::from(peer),
            ConversationType::ServerDm,
            author,
            message.timestamp,
            &targets,
        )?;
        Ok(())
    }

    pub(super) async fn snapshot_channel_receipt_targets(
        &self,
        channel: ChannelId,
        sent_at: u64,
        auth: &mut crate::network::grpc::auth_client::AuthClient,
        channels: &mut crate::network::grpc::channel_client::ChannelClient,
    ) -> Result<(), MessageError> {
        let author = self.current_user_id()?;
        let mut targets = Vec::new();
        for member in channels
            .get_all_channel_members(&channel.to_string())
            .await?
        {
            let peer: UserId = member
                .user_id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid channel member ID".into()))?;
            if peer == author {
                continue;
            }
            let devices = self
                .signal_service
                .ensure_sessions_all_devices(auth, &peer.to_string())
                .await
                .map_err(|error| MessageError::EncryptionFailed(error.to_string()))?;
            targets.extend(devices.into_iter().map(|device| (peer, DeviceId(device))));
        }
        MessageReceiptRepository::new(self.database.clone()).snapshot_for_send(
            channel,
            self.conversation_type(&channel),
            author,
            sent_at,
            &targets,
        )?;
        Ok(())
    }

    pub fn message_receipts(
        &self,
        message_id: MessageId,
    ) -> Result<Vec<DeviceReceipt>, MessageError> {
        Ok(MessageReceiptRepository::new(self.database.clone()).for_message(message_id)?)
    }

    pub fn message_receipt_info(&self, id: MessageId) -> Result<String, MessageError> {
        let message = self
            .message_repo
            .get(&id)?
            .ok_or(MessageError::MessageNotFound(id))?;
        let rows = self.message_receipts(id)?;
        if rows.is_empty() {
            return Ok("Delivery details are not available for this message.".into());
        }
        let show_read = self
            .settings_repo
            .get_bool(keys::SEND_READ_RECEIPTS)?
            .unwrap_or(true);
        let mut lines = vec![format!(
            "Status: {}",
            if !show_read && message.delivery_status == "read" {
                "delivered"
            } else {
                &message.delivery_status
            }
        )];
        for row in rows {
            let name = self
                .user_repo
                .get_profile(&row.recipient_id)?
                .map(|profile| profile.display_name)
                .filter(|name| !name.is_empty())
                .or(self
                    .user_repo
                    .get(&row.recipient_id)?
                    .map(|user| user.username))
                .unwrap_or_else(|| row.recipient_id.to_string());
            let time = |value: Option<u64>| {
                value
                    .and_then(|value| i64::try_from(value).ok())
                    .map(|value| {
                        chrono::DateTime::from_timestamp_nanos(value)
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_else(|| "Pending".into())
            };
            lines.push(format!(
                "\n{} · Device {}\nDelivered: {}\nRead: {}",
                name,
                row.device_id.0,
                time(row.delivered_at),
                if show_read {
                    time(row.read_at)
                } else {
                    "Read receipts off".into()
                }
            ));
        }
        Ok(lines.join("\n"))
    }

    /// `recipient` and `device` MUST come from authenticated transport metadata.
    pub fn apply_peer_receipt(
        &self,
        conversation: ChannelId,
        kind: ConversationType,
        recipient: UserId,
        device: DeviceId,
        receipt_type: i32,
        timestamps: &[u64],
    ) -> Result<(), MessageError> {
        let Some(read) = receipt_stage(&self.settings_repo, receipt_type)? else {
            return Ok(());
        };
        let ids = MessageReceiptRepository::new(self.database.clone()).apply(
            conversation,
            kind,
            self.current_user_id()?,
            recipient,
            device,
            timestamps,
            read,
            crate::util::time::TimestampNanos::now().as_u64(),
        )?;
        for id in ids {
            if let Some(message) = self.message_repo.get(&id)? {
                self.event_bus
                    .emit_domain(DomainEvent::MessageDeliveryChanged {
                        message_id: id,
                        channel_id: conversation,
                        status: message.delivery_status,
                    });
            }
        }
        Ok(())
    }
}

/// The same preference gates sending and observing peer read receipts.
fn receipt_stage(
    settings: &SettingsRepository,
    receipt_type: i32,
) -> Result<Option<bool>, crate::error::StorageError> {
    match receipt_type {
        1 => Ok(Some(false)),
        2 | 3 if settings.get_bool(keys::SEND_READ_RECEIPTS)?.unwrap_or(true) => Ok(Some(true)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn queued_status_events_cannot_regress_acknowledgements() {
        assert_eq!(advance_delivery_status("read", "delivered"), "read");
        assert_eq!(advance_delivery_status("delivered", "sent"), "delivered");
        assert_eq!(advance_delivery_status("delivered", "read"), "read");
        assert_eq!(advance_delivery_status("sending", "sent"), "sent");
    }

    #[test]
    fn privacy_disables_read_and_viewed_but_keeps_delivery() {
        let temp = tempfile::TempDir::new().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&temp.path().join("privacy.db"), "test").unwrap(),
        );
        db.run_migrations().unwrap();
        let settings = SettingsRepository::new(db);
        assert_eq!(receipt_stage(&settings, 2).unwrap(), Some(true));
        settings.set_bool(keys::SEND_READ_RECEIPTS, false).unwrap();
        assert_eq!(receipt_stage(&settings, 1).unwrap(), Some(false));
        assert_eq!(receipt_stage(&settings, 2).unwrap(), None);
        assert_eq!(receipt_stage(&settings, 3).unwrap(), None);
        assert_eq!(receipt_stage(&settings, 0).unwrap(), None);
        assert_eq!(receipt_stage(&settings, 99).unwrap(), None);
    }
}
