//! Message forwarding operations.
//!
//! Forward messages between channels.

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{Message, MessageContent};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::services::current_user::CurrentUserProvider;

use super::conversion::{conversation_type_for_channel, proto_to_domain_message};
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
        let sender_id = self.settings_repo.current_user_id()?;

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

        // Copy attachment IDs from source if any
        let attachment_ids: Vec<String> = source
            .attachments
            .iter()
            .map(|id| id.to_string())
            .collect();

        let content = if attachment_ids.is_empty() {
            chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(forwarded_text.clone())),
            }
        } else {
            let files: Vec<chat_v1::FileMetadata> = attachment_ids
                .iter()
                .map(|id| chat_v1::FileMetadata {
                    id: id.clone(),
                    ..Default::default()
                })
                .collect();
            chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Mixed(
                    chat_v1::message::MixedContent {
                        text: forwarded_text.clone(),
                        files,
                    },
                )),
            }
        };

        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(target_channel_id, channel_client)
            .await?;

        let plaintext_bytes = {
            use prost::Message as _;
            content.encode_to_vec()
        };

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            target_channel_id,
            &plaintext_bytes,
            content,
            is_dm,
            require_encryption,
        ).await;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, target_channel_id, &skdm).await?;
        }

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

        // Convert to domain message and copy source attachments.
        // Forward lands in `target_channel_id`; derive ConversationType
        // from that channel (may differ from source's conversation_type).
        let conv_type = conversation_type_for_channel(&self.channel_repo, target_channel_id);
        let mut message = proto_to_domain_message(&proto_message, sender_id, conv_type)?;
        message.attachments = source.attachments.clone();

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(forwarded_text);

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }
}
