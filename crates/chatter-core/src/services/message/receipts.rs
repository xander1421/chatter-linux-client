//! Durable peer receipt application. Privacy and aggregation stay in core.
use super::{MessageError, MessageService};
use crate::domain::ids::{ChannelId, DeviceId, MessageId, UserId};
use crate::domain::message::ConversationType;
use crate::inbox::DomainEvent;
use crate::storage::repositories::message_receipt_repo::{DeviceReceipt, MessageReceiptRepository};
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

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
    pub(crate) fn snapshot_dm_content_targets(
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
        let mut author_is_member = false;
        let members = channels
            .get_all_channel_members(&channel.to_string())
            .await?;
        // A successful membership fetch is authoritative. Never distribute
        // keys or resume a parked post after our membership was revoked.
        if !members
            .iter()
            .any(|member| member.user_id == author.to_string())
        {
            return Err(MessageError::NotAuthor);
        }
        for member in members {
            let peer: UserId = member
                .user_id
                .parse()
                .map_err(|_| MessageError::InvalidContent("invalid channel member ID".into()))?;
            if peer == author {
                author_is_member = true;
                continue;
            }
            let devices = self
                .signal_service
                .ensure_sessions_all_devices(auth, &peer.to_string())
                .await
                .map_err(|error| match error {
                    crate::error::ServiceError::Network(error) => MessageError::Network(error),
                    other => MessageError::EncryptionFailed(other.to_string()),
                })?;
            targets.extend(devices.into_iter().map(|device| (peer, DeviceId(device))));
        }
        let receipts = MessageReceiptRepository::new(self.database.clone());
        receipts.snapshot_for_send(
            channel,
            self.conversation_type(&channel),
            author,
            sent_at,
            &targets,
        )?;
        // Our other devices read this send through the same sender key, so a
        // resend may go to them too, but only to the ones that exist now: a
        // device linked later gets nothing, as for any member. If the list
        // cannot be fetched, nothing is recorded and no own device gets a
        // resend of this message.
        if author_is_member {
            let current = self
                .settings_repo
                .get::<u32>(keys::CURRENT_DEVICE_ID)
                .ok()
                .flatten();
            let own: Vec<DeviceId> = self
                .own_device_ids()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|device| Some(*device) != current)
                .map(DeviceId)
                .collect();
            receipts.snapshot_own_devices_for_send(
                channel,
                self.conversation_type(&channel),
                author,
                sent_at,
                &own,
            )?;
        }
        Ok(())
    }

    /// Preserve the originating device's receipt scope in its own-device transcript.
    pub(super) fn populate_sync_receipt_targets(
        &self,
        content: &mut crate::proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{SyncSentReceiptTarget, content::Content};
        let Some(Content::SyncSent(sync)) = content.content.as_mut() else {
            return Ok(());
        };
        let peer: UserId = sync
            .destination_user_id
            .parse()
            .map_err(|_| MessageError::InvalidContent("invalid sync destination".into()))?;
        sync.receipt_targets = MessageReceiptRepository::new(self.database.clone())
            .targets_for_send(
                ChannelId::from(peer),
                ConversationType::ServerDm,
                self.current_user_id()?,
                sync.timestamp,
            )?
            .into_iter()
            .map(|(user, device)| SyncSentReceiptTarget {
                user_id: user.to_string(),
                device_id: device.0,
            })
            .collect();
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
        let show_read = self
            .settings_repo
            .get_bool(keys::SEND_READ_RECEIPTS)?
            .unwrap_or(true);
        let mut lines = vec![message_receipt_summary(&message, show_read)];
        if rows.is_empty() {
            lines.push("Per-device delivery times are not available for this message.".into());
        }
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

fn message_receipt_summary(message: &crate::domain::message::Message, show_read: bool) -> String {
    let status = match message.delivery_status.as_str() {
        "read" if !show_read => "delivered",
        "" => "Unknown",
        status => status,
    };
    let sent = i64::try_from(message.sent_at)
        .ok()
        .map(chrono::DateTime::from_timestamp_nanos)
        .unwrap_or(message.created_at)
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    format!("Sent: {sent}\nStatus: {status}")
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

    #[tokio::test]
    async fn sync_sent_ingestion_preserves_phone_targets_and_applies_tablet_receipts() {
        use crate::domain::message::Message;
        use crate::proto::content_v1::content;
        use crate::services::gateway::MessageGateway;

        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        let phone_dir = tempfile::tempdir().unwrap();
        let tablet_dir = tempfile::tempdir().unwrap();
        let build = |path: &std::path::Path| {
            crate::client::ChatterClient::builder()
                .server_url("https://localhost:1")
                .data_dir(path.to_str().unwrap())
                .build()
                .unwrap()
        };
        let phone = build(phone_dir.path());
        let tablet = build(tablet_dir.path());
        let author = UserId::new();
        let peer = UserId::new();
        let channel = ChannelId::from(peer);
        for client in [&phone, &tablet] {
            client
                .messages()
                .settings_repo
                .set_string(keys::CURRENT_USER_ID, &author.to_string())
                .unwrap();
        }
        let target = tablet.messages();
        let gateway = MessageGateway::new(
            target.signal_service.as_ref().clone(),
            target.message_repo.clone(),
            target.channel_repo.clone(),
            target.user_repo.clone(),
            target.settings_repo.clone(),
            target.solicited_resends.clone(),
        );
        for (timestamp, early) in [(123, false), (124, true)] {
            let original = crate::services::build_text_content("phone message", timestamp);
            phone
                .messages()
                .snapshot_dm_content_targets(peer, &original, &[1, 2])
                .unwrap();
            let mut copy = super::super::dm::sync_sent_copy(&peer, &original).unwrap();
            phone
                .messages()
                .populate_sync_receipt_targets(&mut copy)
                .unwrap();
            let Some(content::Content::SyncSent(sync)) = copy.content else {
                panic!("expected SyncSent")
            };
            assert_eq!(sync.receipt_targets.len(), 2);
            assert!(
                gateway
                    .store_synced_sent_message(sync.clone(), peer)
                    .await
                    .is_err()
            );
            let mut wrong_target = sync.clone();
            wrong_target.receipt_targets[0].user_id = UserId::new().to_string();
            assert!(
                gateway
                    .store_synced_sent_message(wrong_target, author)
                    .await
                    .is_err()
            );
            if early {
                target
                    .apply_peer_receipt(
                        channel,
                        ConversationType::ServerDm,
                        peer,
                        DeviceId(2),
                        2,
                        &[timestamp],
                    )
                    .unwrap();
            }
            gateway
                .store_synced_sent_message(sync, author)
                .await
                .unwrap();
            if !early {
                target
                    .apply_peer_receipt(
                        channel,
                        ConversationType::ServerDm,
                        peer,
                        DeviceId(2),
                        2,
                        &[timestamp],
                    )
                    .unwrap();
            }
            let stored = target
                .message_repo
                .get_by_sender_timestamp(&author, timestamp)
                .unwrap()
                .unwrap();
            assert_eq!(stored.delivery_status, "read");
            assert_eq!(target.message_receipts(stored.id).unwrap().len(), 2);
        }
        let mut old = Message::new_text(
            channel,
            ConversationType::ServerDm,
            Some(author),
            "old".into(),
            125,
        );
        old.delivery_status = "delivered".into();
        target.message_repo.insert_outgoing(&old).unwrap();
        let info = target.message_receipt_info(old.id).unwrap();
        assert!(info.contains("Sent:"));
        assert!(info.contains("Status: delivered"));
    }

    #[test]
    fn message_info_without_device_evidence_preserves_known_facts() {
        let mut message = crate::domain::message::Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            "old message".into(),
            1_700_000_000_000_000_000,
        );
        message.delivery_status = "read".into();
        let summary = message_receipt_summary(&message, true);
        assert!(summary.contains("Sent: 2023-"));
        assert!(summary.contains("Status: read"));
        assert!(message_receipt_summary(&message, false).contains("Status: delivered"));
    }

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
