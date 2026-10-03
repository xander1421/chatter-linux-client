//! Saved messages (Note to Self) operations.
//!
//! Follows Signal's approach:
//! - Store locally on the sending device
//! - If multi-device: send SavedMessageSync to other devices via Signal Protocol
//! - No application-level encryption (Signal Protocol device sessions provide it)
//! - No profile key usage (profile key is shared with friends — wrong threat model)

use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message};
use crate::network::grpc::message_client::MessageClient;
use crate::services::current_user::CurrentUserProvider;
use crate::services::gateway::build_saved_message_sync_content;
use crate::util::time::TimestampNanos;

use super::core::proto_to_domain_message_with_sender;
use super::{MessageError, MessageService};

impl MessageService {
    /// Send a message to the SavedMessages channel (Note to Self).
    ///
    /// Follows Signal Desktop's approach:
    /// - Always store locally first
    /// - Build a SavedMessageSync Content proto and send it to our other
    ///   devices like any sync transcript (`sync_to_own_devices`)
    pub async fn save_message(
        &self,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
    ) -> Result<Message, MessageError> {
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Create and store the message locally
        let channel_id = ChannelId::from(sender_id); // SavedMessages channel_id = user_id
        let sent_at = TimestampNanos::now().as_u64();
        let mut message = Message::new_text(channel_id, ConversationType::SavedMessages, Some(sender_id), trimmed_text.to_string(), sent_at);
        message.attachments = attachment_ids.clone();
        self.message_repo.insert_outgoing(&message)?;

        // Sync to other devices. Failures are logged there and non-fatal:
        // the message is stored locally.
        self.sync_saved_message_to_devices(&message.id, trimmed_text, sent_at, &attachment_ids)
            .await;

        Ok(message)
    }

    /// Sync a saved message to the user's other linked devices, through the
    /// one own-device path every sync transcript takes
    /// ([`Self::sync_to_own_devices`]): padded, this device excluded, the
    /// device set checked by the server and reconciled when it changed, and
    /// in Shield only devices we already hold a session with. Best effort;
    /// returns how many devices it reached.
    async fn sync_saved_message_to_devices(
        &self,
        message_id: &MessageId,
        text: &str,
        sent_at: u64,
        attachment_ids: &[AttachmentId],
    ) -> usize {
        let content = build_saved_message_sync_content(
            &message_id.to_string(),
            text,
            sent_at,
            attachment_ids.iter().map(|id| id.to_string()).collect(),
        );
        let synced = self.sync_to_own_devices(&mut self.auth_client(), content).await;
        tracing::debug!(message_id = %message_id, synced, "SavedMessages: own-device sync done");
        synced
    }

    /// Store a saved message received via sync from another device.
    ///
    /// Called by the inbox handler when a SavedMessageSync is received.
    pub fn store_synced_saved_message(
        &self,
        message_id: &str,
        text: &str,
        sent_at: u64,
        attachment_ids: Vec<String>,
    ) -> Result<Message, MessageError> {
        let sender_id = self.settings_repo.current_user_id()?;
        let channel_id = ChannelId::from(sender_id);

        let parsed_id: MessageId = message_id.parse()
            .map_err(|_| MessageError::InvalidContent(format!("invalid message ID: {}", message_id)))?;

        // Check for duplicate (idempotent)
        if self.message_repo.get(&parsed_id)?.is_some() {
            tracing::debug!(message_id, "SavedMessages sync: message already exists, skipping");
            return Ok(self.message_repo.get(&parsed_id)?.unwrap());
        }

        let att_ids: Vec<AttachmentId> = attachment_ids.iter()
            .map(|s| AttachmentId::from_cdn_key(s))
            .collect();

        let mut message = Message::new_text(channel_id, ConversationType::SavedMessages, Some(sender_id), text.to_string(), sent_at);
        message.id = parsed_id;
        message.attachments = att_ids;

        self.message_repo.insert(&message)?;

        tracing::info!(
            message_id,
            "SavedMessages: stored synced message from another device"
        );

        Ok(message)
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
        let _ = self.settings_repo.current_user_id()?;

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

            // Saved-messages RPC only returns messages in the current
            // user's personal vault channel — ConversationType is
            // unambiguously SavedMessages regardless of what the channel
            // row says.
            let message = proto_to_domain_message_with_sender(
                proto,
                sender_id,
                crate::domain::message::ConversationType::SavedMessages,
            )?;

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
