//! Server-managed DM operations.
//!
//! For DMs with server-known friends (`ChannelType::DirectMessage` channels
//! created by the server's friend-accept Temporal workflow), we send via
//! `MessageService::SendMessage` gRPC and persist locally with
//! `ConversationType::ServerDm`. Plaintext on the wire is acceptable here:
//! the server already saw the friendship gRPC, so sealed-sender mailbox
//! transport buys no privacy.
//!
//! Private-friend DMs (mailbox-only, no server channel) continue to use
//! the `dm.rs` mailbox path and persist as `ConversationType::MailboxDm`.

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};
use crate::proto::chat_v1;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::current_user::CurrentUserProvider;

use super::{DmSendResult, MessageError, MessageService};

impl MessageService {
    /// Send a server-managed DM via gRPC `MessageService::SendMessage`.
    ///
    /// Used for friend DMs where the server already knows the friendship
    /// (and therefore created the DM channel). The wire payload is plaintext
    /// text in `chat_v1::message::content::Type::Text`, mirroring the
    /// non-encrypted channel send path.
    ///
    /// Locally the message is persisted with `ConversationType::ServerDm`.
    ///
    /// # Arguments
    /// * `channel_id` - DM channel id (server-assigned, present in the local
    ///   channel repo via the friend-accept event).
    /// * `peer_id` - Peer user id (kept for parity with the mailbox path
    ///   and future use; not currently sent on the wire — server resolves
    ///   the recipient from `channel_id`).
    /// * `text` - Message body.
    /// * `attachment_pointers` - Pre-uploaded attachment pointers. Their
    ///   CDN keys derive `AttachmentId`s for local storage and the matching
    ///   `mentioned_file_ids` field on the request.
    /// * `sent_at` - Caller-generated timestamp (nanoseconds) so UI and
    ///   storage agree.
    pub async fn send_server_dm(
        &self,
        channel_id: &ChannelId,
        peer_id: &UserId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        sent_at: u64,
    ) -> Result<DmSendResult, MessageError> {
        let _ = peer_id; // Server resolves the recipient from channel_id.

        tracing::info!(
            channel_id = %channel_id,
            peer_id = %peer_id,
            text_len = text.len(),
            attachment_count = attachment_pointers.len(),
            sent_at,
            "MessageService: send_server_dm called"
        );

        let trimmed = text.trim();
        if trimmed.is_empty() && attachment_pointers.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        let current_user_id = self.settings_repo.current_user_id()?;

        // Wire payload: plaintext text. Mirrors the channel send path's
        // `Content::Text` shape — server doesn't encrypt anyway, and
        // we don't carry per-attachment keys on the wire (attachments
        // ride via mentioned_file_ids; the server-side blob layer
        // owns delivery).
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed.to_string())),
        };

        // Derive AttachmentIds + mentioned_file_ids from pointers.
        let attachment_ids: Vec<AttachmentId> = attachment_pointers
            .iter()
            .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();
        let mentioned_file_ids: Vec<String> = attachment_ids.iter().map(|id| id.to_string()).collect();

        // Send via gRPC MessageService::SendMessage.
        let mut message_client = self.message_client();
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content,
                None,  // parent_id (no threads in DMs)
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options (plaintext)
                vec![], // mentioned_integrations
                false,  // targets_ai_bot
                mentioned_file_ids,
            )
            .await?;

        tracing::info!(
            channel_id = %channel_id,
            message_id = %proto_message.id,
            "[SEND_SERVER_DM] gRPC SendMessage success"
        );

        // Build the locally-stored message with ConversationType::ServerDm.
        // Use caller-supplied `sent_at` so the UI optimistic row matches
        // the persisted row. Use the server's message id when parseable
        // (so receipts/edits/deletes referencing the server id resolve);
        // otherwise fall back to a fresh local id.
        let mut message = Message::new_text(
            *channel_id,
            ConversationType::ServerDm,
            Some(current_user_id),
            trimmed.to_string(),
            sent_at,
        );

        if let Ok(server_id) = proto_message.id.parse::<crate::domain::ids::MessageId>() {
            message.id = server_id;
        }

        if !attachment_ids.is_empty() {
            message.attachments = attachment_ids.clone();
            // Reflect attachments in the stored content shape so message
            // rendering picks them up the same way the channel path does.
            message.content = if trimmed.is_empty() {
                MessageContent::Mixed(MixedContent::attachments_only(attachment_ids.clone()))
            } else {
                MessageContent::Mixed(MixedContent::new(trimmed.to_string(), attachment_ids.clone()))
            };
        }

        if let Err(e) = self.message_repo.insert(&message) {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to store sent ServerDm locally"
            );
        }

        Ok(DmSendResult {
            message: Some(message),
            attachment_ids,
            gif_attachment_ids: vec![],
            sent_at,
            bootstrap_initiated: false,
        })
    }
}
