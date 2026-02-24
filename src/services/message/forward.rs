//! Message forwarding operations.
//!
//! Forward messages between channels.

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{Message, MessageContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;

use super::conversion::proto_to_domain_message;
use super::{MessageError, MessageService};

impl MessageService {
    /// Forward a message to another channel.
    pub async fn forward_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        source_message_id: &MessageId,
        target_channel_id: &ChannelId,
        comment: Option<&str>,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.current_user_id()?;

        // Get source message from local storage
        let source = self
            .message_repo
            .get(source_message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*source_message_id))?;

        // Extract text content from source message
        let original_text = source.as_text().unwrap_or("[No text content]");

        // Build forwarded content with attribution
        let forwarded_text = match comment {
            Some(c) if !c.trim().is_empty() => {
                format!("{}\n\n---\nForwarded:\n{}", c.trim(), original_text)
            }
            _ => format!("Forwarded:\n{}", original_text),
        };

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(forwarded_text.clone())),
        };

        // Get encryption settings from channel config
        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(target_channel_id, channel_client)
            .await?;

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            target_channel_id,
            forwarded_text.as_bytes(),
            content,
            is_dm,
            require_encryption,
        ).await;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, target_channel_id, &skdm).await?;
        }

        // Copy attachment IDs from source if any
        let attachment_ids: Vec<String> = source
            .attachments
            .iter()
            .map(|id| id.to_string())
            .collect();

        // Call server
        let proto_message = message_client
            .send_message(
                &target_channel_id.to_string(),
                content_to_send,
                None,  // parent_id
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                Vec::new(), // mentioned_integrations
                false, // targets_ai_bot
                attachment_ids,
            )
            .await?;

        // Convert to domain message and copy source attachments
        let mut message = proto_to_domain_message(&proto_message, sender_id)?;
        message.attachments = source.attachments.clone();

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(forwarded_text);

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }
}
