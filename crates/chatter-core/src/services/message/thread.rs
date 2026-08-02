//! Thread operations.
//!
//! Fetch thread messages. Thread replies are SENT through the normal message
//! path with `thread_id` set (`MessageService::send_message`) so they pick up
//! the wire thread id, fanout, and disappearing-timer handling.

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::Message;
use crate::network::grpc::message_client::MessageClient;
use crate::services::current_user::CurrentUserProvider;

use super::conversion::conversation_type_for_channel;
use super::core::proto_to_domain_message_with_sender;
use super::{MessageError, MessageService};

impl MessageService {
    /// Get messages from a thread.
    ///
    /// Fetches messages from a specific thread.
    pub async fn get_thread_messages(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        thread_root_id: &MessageId,
        limit: u32,
        before: Option<&MessageId>,
    ) -> Result<Vec<Message>, MessageError> {
        use crate::proto::chat_v1::MessageSortDirection;

        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Fetch from server with thread_root_id filter
        let response = message_client
            .get_messages(
                &channel_id.to_string(),
                limit as i32,
                Some(&thread_root_id.to_string()), // thread_root_id filter
                None, // page_token
                MessageSortDirection::SortNewestFirst,
                before.map(|id| id.to_string()).as_deref(),
                None, // reference_time
            )
            .await?;

        // Convert proto messages to domain messages and store locally.
        // Every message in a thread belongs to the same channel, so
        // derive ConversationType once.
        let conv_type = conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut messages = Vec::with_capacity(response.messages.len());
        for proto in &response.messages {
            // Parse sender ID from proto author_id
            let sender_id: Option<UserId> = if proto.author_id.is_empty() {
                None
            } else {
                proto.author_id.parse().ok()
            };

            let message = proto_to_domain_message_with_sender(proto, sender_id, conv_type.clone())?;

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
