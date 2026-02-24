//! Pin operations.
//!
//! Pin and unpin messages, list pinned messages.

use chrono::Utc;

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::Message;
use crate::network::grpc::message_client::MessageClient;

use super::core::proto_to_domain_message_with_sender;
use super::{MessageError, MessageService};

impl MessageService {
    /// Pin a message in a channel.
    pub async fn pin_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let proto_message = message_client
            .pin_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Parse sender ID from proto
        let sender_id: Option<UserId> = if proto_message.author_id.is_empty() {
            None
        } else {
            proto_message.author_id.parse().ok()
        };

        // Convert to domain message
        let message = proto_to_domain_message_with_sender(&proto_message, sender_id)?;

        // Update local storage
        match self.message_repo.get(&message.id)? {
            Some(_) => self.message_repo.update(&message)?,
            None => self.message_repo.insert(&message)?,
        }

        Ok(message)
    }

    /// Unpin a message from a channel.
    pub async fn unpin_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<(), MessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        message_client
            .unpin_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Update local storage - set is_pinned = false
        if let Some(mut existing) = self.message_repo.get(message_id)? {
            existing.is_pinned = false;
            existing.updated_at = Utc::now();
            self.message_repo.update(&existing)?;
        }

        Ok(())
    }

    /// List pinned messages in a channel.
    pub async fn list_pinned_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server
        let response = message_client
            .list_pinned_messages(&channel_id.to_string(), limit as i32)
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

            // Upsert to local storage
            match self.message_repo.get(&message.id)? {
                Some(_) => self.message_repo.update(&message)?,
                None => self.message_repo.insert(&message)?,
            }

            messages.push(message);
        }

        Ok(messages)
    }
}
