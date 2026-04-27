//! Thread operations.
//!
//! Send thread replies and fetch thread messages.

use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{Message, MessageContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::services::current_user::CurrentUserProvider;

use super::conversion::conversation_type_for_channel;
use super::core::proto_to_domain_message_with_sender;
use super::{MessageError, MessageService};

impl MessageService {
    /// Send a reply within a thread.
    ///
    /// Creates a new message in an existing thread. The parent_id is set to
    /// the thread root message ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_thread_reply(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        text: &str,
        thread_root_id: &MessageId,
        attachment_ids: Vec<AttachmentId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "thread reply must have text or attachments".to_string(),
            ));
        }

        let content = if attachment_ids.is_empty() {
            chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
            }
        } else {
            let files: Vec<chat_v1::FileMetadata> = attachment_ids
                .iter()
                .map(|id| chat_v1::FileMetadata {
                    id: id.to_string(),
                    ..Default::default()
                })
                .collect();
            chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Mixed(
                    chat_v1::message::MixedContent {
                        text: trimmed_text.to_string(),
                        files,
                    },
                )),
            }
        };

        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        let plaintext_bytes = {
            use prost::Message as _;
            content.encode_to_vec()
        };

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            channel_id,
            &plaintext_bytes,
            content,
            is_dm,
            require_encryption,
        ).await;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
        }

        // Call server - parent_id is thread_root_id for thread replies
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send,
                Some(&thread_root_id.to_string()), // parent_id = thread_root_id
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and preserve attachment IDs.
        // Thread reply lives in the same channel as the root; derive
        // ConversationType from the channel row.
        let conv_type = conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut message = super::core::proto_to_domain_message(&proto_message, sender_id, conv_type)?;
        message.attachments = attachment_ids;

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

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
