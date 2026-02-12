//! Group mailbox polling.
//!
//! Handles polling and processing messages from per-group ephemeral mailboxes
//! (NDSS '21 paper implementation for privacy-preserving group messaging).

use std::sync::Arc;

use prost::Message as ProstMessage;
use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::Message;
use crate::domain::UserId;
use crate::inbox::events::InboxEvent;
use crate::mailbox::group::{GroupCoordinator, GroupFanout};
use crate::proto::chat_v1::EncryptedData;
use crate::proto::content_v1::DataMessage;
use crate::services::message_gateway::{MessageGateway, RouteResult};
use crate::storage::repositories::settings_repo::keys;
use crate::storage::{ChannelRepository, MessageRepository, SettingsRepository};

/// Poll all group mailboxes for new messages.
///
/// Group messages are received via per-group ephemeral mailboxes (NDSS '21 paper).
/// Messages are encrypted with sender keys and routed through anonymous mailboxes
/// to prevent server-side correlation of group membership.
///
/// This method handles:
/// - Sender key encrypted channel messages
/// - GroupMailboxAddress announcements (member mailbox exchange)
#[allow(clippy::too_many_arguments)]
pub async fn poll_group_mailboxes(
    group_fanout: &Arc<GroupFanout>,
    group_coordinator: &Arc<GroupCoordinator>,
    message_gateway: &Arc<MessageGateway>,
    event_tx: &broadcast::Sender<InboxEvent>,
    settings_repo: &Arc<SettingsRepository>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
) {
    // Get current user ID for message attribution
    let current_user_id: Option<UserId> = settings_repo
        .get_string(keys::CURRENT_USER_ID)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());

    match group_fanout.poll_all_group_mailboxes().await {
        Ok(poll_results) => {
            let mut total_groups = 0u32;
            let mut total_messages = 0u32;

            for (group_id, poll_result) in poll_results {
                total_groups += 1;
                let message_count = poll_result.messages.len();

                if message_count == 0 {
                    continue;
                }

                // Collect message IDs for acknowledgment
                let message_ids = poll_result.message_ids();

                for msg in poll_result.messages {
                    total_messages += 1;

                    // Try to decode as EncryptedData (sender key encrypted)
                    if let Ok(encrypted_data) =
                        EncryptedData::decode(msg.encrypted_envelope.as_slice())
                    {
                        // Extract sender from associated_data
                        let sender_info = extract_sender_from_encrypted_data(&encrypted_data);

                        if let Some(sender_id) = sender_info {
                            process_group_message(
                                &encrypted_data,
                                &sender_id,
                                group_id,
                                &msg.message_id,
                                message_gateway,
                                group_coordinator,
                                event_tx,
                                message_repo,
                                channel_repo,
                                &current_user_id,
                            )
                            .await;
                        } else {
                            tracing::warn!(
                                group_id = %group_id,
                                "Could not extract sender from group mailbox message"
                            );
                        }
                    } else {
                        tracing::warn!(
                            group_id = %group_id,
                            message_id = %msg.message_id,
                            "Failed to decode group mailbox message as EncryptedData"
                        );
                    }
                }

                // Acknowledge processed messages
                if !message_ids.is_empty() {
                    if let Err(e) = group_fanout
                        .acknowledge_group_messages(&group_id, message_ids)
                        .await
                    {
                        tracing::warn!(
                            group_id = %group_id,
                            error = %e,
                            "Failed to acknowledge group mailbox messages"
                        );
                    }
                }
            }

            if total_messages > 0 || total_groups > 0 {
                let _ = event_tx.send(InboxEvent::GroupMailboxPollCompleted {
                    total_groups,
                    total_messages,
                });
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "Group mailbox poll failed (may be normal if no active groups)");
        }
    }
}

/// Process a single group mailbox message.
#[allow(clippy::too_many_arguments)]
async fn process_group_message(
    encrypted_data: &EncryptedData,
    sender_id: &UserId,
    group_id: ChannelId,
    message_id: &str,
    message_gateway: &Arc<MessageGateway>,
    group_coordinator: &Arc<GroupCoordinator>,
    event_tx: &broadcast::Sender<InboxEvent>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    current_user_id: &Option<UserId>,
) {
    match message_gateway
        .route_encrypted_message(encrypted_data, sender_id, 1)
        .await
    {
        Ok(RouteResult::Decrypted(plaintext)) => {
            // Parse as DataMessage and store
            if let Ok(data_message) = DataMessage::decode(plaintext.as_slice()) {
                if current_user_id.is_some() {
                    let msg_id = message_id
                        .parse::<MessageId>()
                        .unwrap_or_else(|_| MessageId::new());

                    let mut message =
                        Message::new_text(group_id, Some(*sender_id), data_message.body.clone());
                    message.id = msg_id;

                    if data_message.timestamp > 0 {
                        if let Some(dt) =
                            chrono::DateTime::from_timestamp_millis(data_message.timestamp as i64)
                        {
                            message.created_at = dt;
                            message.updated_at = dt;
                        }
                    }

                    match message_repo.insert_or_ignore(&message) {
                        Ok(true) => {
                            if let Ok(new_count) = channel_repo.increment_unread_count(&group_id) {
                                tracing::debug!(
                                    channel_id = %group_id,
                                    unread_count = new_count,
                                    "Incremented group unread count (via mailbox)"
                                );
                            }

                            // Emit ChannelMessageReceived for UI updates
                            let _ = event_tx.send(InboxEvent::ChannelMessageReceived {
                                channel_id: group_id,
                                sender_id: *sender_id,
                                message_id: message_id.to_string(),
                                text: data_message.body.clone(),
                                timestamp: data_message.timestamp,
                            });
                        }
                        Ok(false) => {
                            tracing::debug!(
                                message_id = %message.id,
                                "Group mailbox message already exists (skipped duplicate)"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                message_id = %message.id,
                                error = %e,
                                "Failed to store group mailbox message"
                            );
                        }
                    }
                }
            }
        }
        Ok(RouteResult::MailboxAnnouncement {
            channel_id,
            announcement,
        }) => {
            // Gateway detected GroupMailboxAddress - process via coordinator
            if let Err(e) = group_coordinator.process_announcement(*sender_id, &announcement) {
                tracing::error!(
                    channel_id = %channel_id,
                    sender_id = %sender_id,
                    error = %e,
                    "Failed to process group mailbox announcement"
                );
            } else {
                tracing::info!(
                    channel_id = %channel_id,
                    sender_id = %sender_id,
                    "Processed group mailbox announcement from group mailbox"
                );
                let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                    channel_id,
                    member_id: *sender_id,
                    mailbox_pk: announcement.mailbox_pk.clone(),
                    timestamp: announcement.timestamp,
                });
            }
        }
        Ok(RouteResult::ControlContent(_plaintext)) => {
            tracing::debug!(
                group_id = %group_id,
                "Group mailbox control content received"
            );
        }
        Ok(RouteResult::Processed) => {
            tracing::debug!(
                group_id = %group_id,
                "Group mailbox sender key control message processed"
            );
        }
        Ok(RouteResult::NeedsSenderKey {
            channel_id,
            sender_id,
        }) => {
            tracing::info!(
                group_id = %group_id,
                channel_id = %channel_id,
                sender_id = %sender_id,
                "Need sender key for group mailbox message"
            );
        }
        Err(e) => {
            tracing::error!(
                group_id = %group_id,
                error = %e,
                "Failed to decrypt group mailbox message"
            );
        }
    }
}

/// Extract sender UserId from EncryptedData associated_data.
///
/// The associated_data may contain sender info in format:
/// "sender:{user_id}:channel:{channel_id}"
pub fn extract_sender_from_encrypted_data(encrypted_data: &EncryptedData) -> Option<UserId> {
    let associated_str = String::from_utf8_lossy(&encrypted_data.associated_data);
    if associated_str.starts_with("sender:") {
        let parts: Vec<&str> = associated_str.split(':').collect();
        if parts.len() >= 2 {
            return parts[1].parse().ok();
        }
    }
    None
}
