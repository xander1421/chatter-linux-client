//! Saved messages (Note to Self) operations.
//!
//! Send and retrieve saved messages, with multi-device sync support.

use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use crate::domain::message::Message;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::storage::repositories::settings_repo::keys;
use crate::util::time::TimestampNanos;

use super::core::proto_to_domain_message_with_sender;
use super::{MessageError, MessageService};

impl MessageService {
    /// Send a message to the SavedMessages channel (Note to Self).
    ///
    /// Follows Signal's approach:
    /// - If only one device (this device): store locally only
    /// - If multiple devices: send encrypted sync message to all devices
    pub async fn send_saved_message(
        &self,
        auth_client: &mut AuthClient,
        _message_client: &mut MessageClient, // Reserved for multi-device sync
        text: &str,
        attachment_ids: Vec<AttachmentId>,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

        // Validate content
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Get current device ID
        let current_device_id: u32 = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .unwrap_or(1);

        // List all our devices to determine if we need to sync
        let devices = auth_client.list_devices().await?;
        let device_count = devices.devices.len();

        tracing::debug!(
            "SavedMessages: device_count={}, current_device_id={}",
            device_count,
            current_device_id
        );

        // Create the message locally first
        let channel_id = ChannelId::from(sender_id); // SavedMessages channel_id = user_id

        // Generate timestamp ONCE - used everywhere
        let sent_at = TimestampNanos::now().as_u64();
        let mut message = Message::new_text(channel_id, Some(sender_id), trimmed_text.to_string(), sent_at);
        message.attachments = attachment_ids;

        // Store locally
        self.message_repo.insert(&message)?;

        if device_count <= 1 {
            // Only this device - we're done (local-only storage)
            tracing::info!(
                "SavedMessages: stored locally only (single device), message_id={}",
                message.id
            );
        } else {
            // Multiple devices - need to sync to other devices
            // Signal uses SentTranscript sync messages for this
            //
            // TODO: Implement proper multi-device sync:
            // 1. Create SentTranscript proto message
            // 2. Encrypt for each linked device using Signal Protocol
            // 3. Send via device-specific queue (requires server support)
            //
            // For now, message is stored locally only. Other devices won't see it
            // until proper sync is implemented.
            let other_device_ids: Vec<u32> = devices
                .devices
                .iter()
                .filter(|d| d.device_id != current_device_id)
                .map(|d| d.device_id)
                .collect();

            tracing::warn!(
                "SavedMessages: multi-device sync not yet implemented. \
                 Message stored locally only. Other devices {:?} won't see message_id={}",
                other_device_ids,
                message.id
            );
        }

        Ok(message)
    }

    /// Send a message to SavedMessages (simplified API - no client params).
    pub async fn save_message(
        &self,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
    ) -> Result<Message, MessageError> {
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        self.send_saved_message(&mut auth_client, &mut message_client, text, attachment_ids)
            .await
    }

    /// Get messages from a channel.
    ///
    /// Fetches messages from the server and stores them locally.
    pub async fn get_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        limit: u32,
        before: Option<&MessageId>,
    ) -> Result<Vec<Message>, MessageError> {
        use crate::proto::chat_v1::MessageSortDirection;

        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server
        let response = message_client
            .get_messages(
                &channel_id.to_string(),
                limit as i32,
                None, // thread_root_id
                None, // page_token
                MessageSortDirection::SortNewestFirst,
                before.map(|id| id.to_string()).as_deref(),
                None, // reference_time
            )
            .await?;

        // Convert proto messages to domain messages and store locally
        let mut messages = Vec::with_capacity(response.messages.len());
        for proto in &response.messages {
            // Parse sender ID from proto author_id
            let sender_id: Option<UserId> = if proto.author_id.is_empty() {
                None
            } else {
                proto.author_id.parse().ok()
            };

            let message = proto_to_domain_message_with_sender(proto, sender_id)?;

            // Upsert to local storage (insert or update)
            match self.message_repo.get(&message.id)? {
                Some(_) => self.message_repo.update(&message)?,
                None => self.message_repo.insert(&message)?,
            }

            messages.push(message);
        }

        Ok(messages)
    }
}
