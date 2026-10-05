//! Message gRPC client.
//!
//! Client for message service operations including messaging, reactions,
//! read receipts, typing indicators, pins, search, and streaming.

use crate::network::grpc::transport::GrpcTransport;
use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    AcknowledgeMessagesRequest, AcknowledgeMessagesResponse, DeleteMessageRequest,
    EditMessageRequest, GetDeviceMessagesRequest, GetDeviceMessagesResponse, GetMessagesRequest,
    GetMessagesResponse, ListPinnedMessagesResponse, Message, MessageEvent, MessageSearchRequest,
    MessageSearchResponse, MessageSortDirection, MessageUpdate, SealedDelivery,
    SealedDeviceMessage, SendMessageRequest, SendSealedMessageRequest, SendSealedMessageResponse,
    StreamGlobalEventsRequest, StreamMessageEventsRequest, StreamMessagesRequest,
    StreamMessagesResponse, message_service_client::MessageServiceClient,
};

/// Message service client wrapper.
///
/// Wraps the generated MessageServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut client = MessageClient::new(pool.channel(), pool.auth_clone());
/// let message = client.send_message("channel-123", content, None, false, None, vec![], false, vec![], Vec::new()).await?;
/// ```
pub struct MessageClient {
    inner: MessageServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl MessageClient {
    /// Create a new MessageClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: MessageServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Core Messaging RPCs ====================

    /// Send a message to a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Target channel ID
    /// * `content` - Message content
    /// * `parent_id` - Optional parent message ID for threading
    /// * `ephemeral` - Whether message is ephemeral
    /// * `ephemeral_ttl` - TTL for ephemeral messages
    /// * `mentioned_integrations` - Integration IDs mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    /// * `mentioned_file_ids` - File IDs mentioned in message
    /// * `deletion_token_hash` - SHA-256 of this message's deletion token;
    ///   required (32 bytes) for sealed-sender content, empty otherwise
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &mut self,
        channel_id: &str,
        content: crate::proto::chat_v1::message::Content,
        parent_id: Option<&str>,
        ephemeral: bool,
        ephemeral_ttl: Option<prost_types::Duration>,
        mentioned_integrations: Vec<String>,
        targets_ai_bot: bool,
        mentioned_file_ids: Vec<String>,
        deletion_token_hash: Vec<u8>,
    ) -> Result<Message, NetworkError> {
        let request = SendMessageRequest {
            channel_id: channel_id.to_string(),
            parent_id: parent_id.unwrap_or("").to_string(),
            content: Some(content),
            ephemeral,
            ephemeral_ttl,
            client_request_id: uuid::Uuid::new_v4().to_string(),
            mentioned_integrations,
            targets_ai_bot,
            mentioned_file_ids,
            deletion_token_hash,
        };

        self.inner
            .send_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Edit an existing message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of message to edit
    /// * `channel_id` - Channel containing the message
    /// * `new_content` - Updated message content
    pub async fn edit_message(
        &mut self,
        message_id: &str,
        channel_id: &str,
        new_content: crate::proto::chat_v1::message::Content,
    ) -> Result<Message, NetworkError> {
        let request = EditMessageRequest {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            new_content: Some(new_content),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .edit_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete a message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of message to delete
    /// * `channel_id` - Channel containing the message
    /// * `deletion_token` - the sealed-sender message's deletion token (the
    ///   preimage of the hash sent with it); empty for other messages
    pub async fn delete_message(
        &mut self,
        message_id: &str,
        channel_id: &str,
        deletion_token: Vec<u8>,
    ) -> Result<(), NetworkError> {
        let request = DeleteMessageRequest {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            deletion_token,
        };

        self.inner
            .delete_message(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get messages from a channel with pagination.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to get messages from
    /// * `limit` - Max messages to return
    /// * `thread_root_id` - Optional filter for thread messages
    /// * `page_token` - Pagination token
    /// * `sort_direction` - Sort order
    /// * `reference_message_id` - Optional reference point by message ID
    /// * `reference_time` - Optional reference point by timestamp
    #[allow(clippy::too_many_arguments)]
    pub async fn get_messages(
        &mut self,
        channel_id: &str,
        limit: i32,
        thread_root_id: Option<&str>,
        page_token: Option<&str>,
        sort_direction: MessageSortDirection,
        reference_message_id: Option<&str>,
        reference_time: Option<u64>,
    ) -> Result<GetMessagesResponse, NetworkError> {
        use crate::proto::chat_v1::get_messages_request::ReferencePoint;

        let reference_point = match (reference_message_id, reference_time) {
            (Some(id), _) => Some(ReferencePoint::ReferenceMessageId(id.to_string())),
            (None, Some(time)) => Some(ReferencePoint::ReferenceTime(time)),
            (None, None) => None,
        };

        let request = GetMessagesRequest {
            channel_id: channel_id.to_string(),
            limit,
            thread_root_id: thread_root_id.unwrap_or("").to_string(),
            page_token: page_token.unwrap_or("").to_string(),
            sort_direction: sort_direction.into(),
            reference_point,
            updated_since: None,
        };

        self.inner
            .get_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Recover current message state, tombstones and reactions from the
    /// server's retained change window. The page token freezes its boundaries.
    pub async fn get_message_changes(
        &mut self,
        channel_id: &str,
        page_token: Option<&str>,
    ) -> Result<GetMessagesResponse, NetworkError> {
        self.inner
            .get_messages(GetMessagesRequest {
                channel_id: channel_id.to_string(),
                limit: 50,
                sort_direction: MessageSortDirection::SortOldestFirst.into(),
                page_token: page_token.unwrap_or("").to_string(),
                updated_since: Some(0),
                ..Default::default()
            })
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Stream messages from a channel.
    ///
    /// Returns a long-lived stream of messages for real-time updates.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to stream from
    /// * `resume_from` - Optional message ID to resume from
    ///
    /// Items are messages or the server's `StreamHeartbeat` keepalive (the
    /// first item, then about one a minute); see
    /// `network::streaming::heartbeat`.
    pub async fn stream_messages(
        &mut self,
        channel_id: &str,
        resume_from: Option<&str>,
    ) -> Result<impl Stream<Item = Result<StreamMessagesResponse, NetworkError>>, NetworkError>
    {
        let request = StreamMessagesRequest {
            channel_id: channel_id.to_string(),
            resume_from_message_id: resume_from.unwrap_or("").to_string(),
        };

        let response = self
            .inner
            .stream_messages(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    // ==================== Sealed Sender RPCs ====================

    /// One sealed send to `destination_user_id`, IDENTIFIED (Basic auth):
    /// every device's envelope in one request, queued all together or not at
    /// all. For a sender that holds no Unidentified-Access-Key for the
    /// recipient, and for sends to our own user — the UAK path is
    /// `AnonymousMessageClient::send_sealed_messages_with_uak`.
    ///
    /// A device-set difference the server reports for `delivery` comes back
    /// as [`NetworkError::MismatchedDevices`], with nothing queued.
    /// `client_request_id` is the message GUID every device's copy is stored
    /// under: a repeated send under the same id is not queued twice.
    pub async fn send_sealed_messages(
        &mut self,
        destination_user_id: &str,
        messages: Vec<SealedDeviceMessage>,
        delivery: SealedDelivery,
        client_request_id: &str,
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        let request = SendSealedMessageRequest {
            destination_user_id: destination_user_id.to_string(),
            messages,
            client_request_id: client_request_id.to_string(),
            envelope_type: 0, // Default: server determines from content
            delivery: delivery as i32,
        };

        self.inner
            .send_sealed_message(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))
            .and_then(|r| sealed_send_result(r.into_inner()))
    }

    // ==================== Device Message Queue RPCs ====================

    /// Get queued messages for the authenticated device (Signal-style inbox).
    ///
    /// # Arguments
    ///
    /// * `after_id` - Pagination: fetch messages with queue_id > after_id (0 for start)
    /// * `limit` - Maximum messages to return (default 100, max 1000)
    pub async fn get_device_messages(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<GetDeviceMessagesResponse, NetworkError> {
        let request = GetDeviceMessagesRequest { after_id, limit };

        self.inner
            .get_device_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Acknowledge receipt of messages (deletes from queue).
    ///
    /// # Arguments
    ///
    /// * `message_guids` - List of message GUIDs to acknowledge
    pub async fn acknowledge_messages(
        &mut self,
        message_guids: Vec<String>,
    ) -> Result<AcknowledgeMessagesResponse, NetworkError> {
        let request = AcknowledgeMessagesRequest { message_guids };

        self.inner
            .acknowledge_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Pin/Unpin RPCs ====================

    /// Pin a message in a channel.
    ///
    /// # Arguments
    ///
    /// * `message_id` - Message to pin
    /// * `channel_id` - Channel containing the message
    pub async fn pin_message(
        &mut self,
        message_id: &str,
        channel_id: &str,
    ) -> Result<Message, NetworkError> {
        let request = MessageUpdate {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            update: Some(crate::proto::chat_v1::message_update::Update::Pinned(true)),
            updated_at: 0, // Server will set this
        };

        self.inner
            .pin_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Unpin a message from a channel.
    ///
    /// # Arguments
    ///
    /// * `message_id` - Message to unpin
    /// * `channel_id` - Channel containing the message
    pub async fn unpin_message(
        &mut self,
        message_id: &str,
        channel_id: &str,
    ) -> Result<(), NetworkError> {
        let request = MessageUpdate {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            update: Some(crate::proto::chat_v1::message_update::Update::Pinned(false)),
            updated_at: 0, // Server will set this
        };

        self.inner
            .unpin_message(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List pinned messages in a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to get pinned messages from
    /// * `limit` - Max pinned messages to return
    pub async fn list_pinned_messages(
        &mut self,
        channel_id: &str,
        limit: i32,
    ) -> Result<ListPinnedMessagesResponse, NetworkError> {
        let request = GetMessagesRequest {
            channel_id: channel_id.to_string(),
            limit,
            thread_root_id: String::new(),
            page_token: String::new(),
            sort_direction: MessageSortDirection::SortUnspecified.into(),
            reference_point: None,
            updated_since: None,
        };

        self.inner
            .list_pinned_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Search RPCs ====================

    /// Search messages by content.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Optional channel to search in (empty for global)
    /// * `query` - Search query
    /// * `limit` - Max results to return
    /// * `offset` - Pagination offset
    /// * `from` - Optional start timestamp (Unix milliseconds)
    /// * `to` - Optional end timestamp (Unix milliseconds)
    #[allow(clippy::too_many_arguments)]
    pub async fn search(
        &mut self,
        channel_id: Option<&str>,
        query: &str,
        limit: i32,
        offset: i32,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<MessageSearchResponse, NetworkError> {
        let request = MessageSearchRequest {
            channel_id: channel_id.unwrap_or("").to_string(),
            query: query.to_string(),
            limit,
            offset,
            from: from.unwrap_or(0),
            to: to.unwrap_or(0),
        };

        self.inner
            .search(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Event Streaming RPCs ====================

    /// Stream message events for a single channel.
    ///
    /// Returns a long-lived stream of message events (sent, edited, deleted, reactions, etc.).
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to stream events from
    /// * `resume_from` - Optional event ID to resume from
    pub async fn stream_message_events(
        &mut self,
        channel_id: &str,
        resume_from: Option<&str>,
    ) -> Result<impl Stream<Item = Result<MessageEvent, NetworkError>>, NetworkError> {
        let request = StreamMessageEventsRequest {
            channel_id: channel_id.to_string(),
            resume_from_event_id: resume_from.unwrap_or("").to_string(),
        };

        let response = self
            .inner
            .stream_message_events(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Stream global message events across multiple channels.
    ///
    /// Returns a long-lived stream of message events from all specified channels
    /// (or all user's channels if none specified).
    ///
    /// # Arguments
    ///
    /// * `channel_ids` - Channels to stream from (empty for all user's channels)
    /// * `resume_from` - Optional event ID to resume from
    /// Global resume is a bounded v1 channel-position checkpoint. The
    /// per-channel stream's scalar Redis cursor remains a separate protocol.
    pub async fn stream_global_events(
        &mut self,
        channel_ids: Vec<String>,
        resume_from: Option<&str>,
    ) -> Result<impl Stream<Item = Result<MessageEvent, NetworkError>>, NetworkError> {
        let request = StreamGlobalEventsRequest {
            channel_ids,
            resume_from_event_id: resume_from.unwrap_or("").to_string(),
        };

        let response = self
            .inner
            .stream_global_events(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_message_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
            let _client = MessageClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_message_sort_direction_values() {
        // Verify MessageSortDirection enum is accessible and has expected values
        assert_eq!(MessageSortDirection::SortUnspecified as i32, 0);
        assert_eq!(MessageSortDirection::SortNewestFirst as i32, 1);
        assert_eq!(MessageSortDirection::SortOldestFirst as i32, 2);
    }

    #[test]
    fn test_message_construction() {
        // Verify Message struct can be constructed
        let message = Message {
            id: "msg-123".to_string(),
            channel_id: "channel-456".to_string(),
            author_id: "user-789".to_string(),
            author_device_id: 0,
            parent_id: String::new(),
            content: None,
            timestamp: 1234567890000,
            ephemeral: false,
            ephemeral_ttl: None,
            pinned: false,
            reply_count: 0,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            crypto_status: None,
            updated_at: 0,
            edited: false,
            edited_at: 0,
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            ..Default::default()
        };

        assert_eq!(message.id, "msg-123");
        assert_eq!(message.channel_id, "channel-456");
        assert_eq!(message.author_id, "user-789");
        assert!(!message.ephemeral);
    }

    #[test]
    fn test_message_content_variants() {
        use crate::proto::chat_v1::message::content::Type;

        // Verify Message.Content oneof types can be constructed
        let text_content = crate::proto::chat_v1::message::Content {
            r#type: Some(Type::Text("Hello, world!".to_string())),
        };

        match text_content.r#type {
            Some(Type::Text(t)) => assert_eq!(t, "Hello, world!"),
            _ => panic!("Expected Text variant"),
        }

        // Test encrypted variant
        let encrypted_content = crate::proto::chat_v1::message::Content {
            r#type: Some(Type::Encrypted(crate::proto::chat_v1::EncryptedData {
                version: crate::proto::chat_v1::CryptoVersion::SignalProtocol as i32,
                ciphertext: vec![1, 2, 3, 4],
                associated_data: b"sender_key".to_vec(),
            })),
        };

        match encrypted_content.r#type {
            Some(Type::Encrypted(e)) => {
                assert_eq!(
                    e.version,
                    crate::proto::chat_v1::CryptoVersion::SignalProtocol as i32
                );
            }
            _ => panic!("Expected Encrypted variant"),
        }
    }

    #[test]
    fn test_reaction_construction() {
        // Verify Reaction struct can be constructed
        use crate::proto::content_v1::Reaction;

        let reaction = Reaction {
            emoji: "👍".to_string(),
            remove: false,
            target_author_aci: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            target_sent_timestamp: 1704067200_000_000_000,
            timestamp: 1234567890000,
        };

        assert_eq!(reaction.emoji, "👍");
        assert!(!reaction.remove);
        assert_eq!(reaction.timestamp, 1234567890000);
    }

    #[test]
    fn test_message_event_variants() {
        // Verify MessageEvent struct and its event variants
        use crate::proto::chat_v1::message_event::Event;

        let msg = Message {
            id: "msg-123".to_string(),
            channel_id: "channel-456".to_string(),
            author_id: "user-789".to_string(),
            author_device_id: 0,
            parent_id: String::new(),
            content: None,
            timestamp: 1234567890000,
            ephemeral: false,
            ephemeral_ttl: None,
            pinned: false,
            reply_count: 0,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            crypto_status: None,
            updated_at: 0,
            edited: false,
            edited_at: 0,
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            ..Default::default()
        };

        let event = MessageEvent {
            event_id: "event-001".to_string(),
            timestamp: 1234567890000,
            channel_id: "channel-456".to_string(),
            event: Some(Event::MessageSent(msg)),
        };

        assert_eq!(event.event_id, "event-001");
        assert_eq!(event.channel_id, "channel-456");
        assert!(matches!(event.event, Some(Event::MessageSent(_))));
    }
}

/// A `SendSealedMessage` answer that reports a device-set mismatch is a send
/// that did not happen: make it an error no caller can read as delivered.
pub(crate) fn sealed_send_result(
    response: SendSealedMessageResponse,
) -> Result<SendSealedMessageResponse, NetworkError> {
    match response.mismatched_devices {
        Some(m) => Err(NetworkError::MismatchedDevices {
            missing: m.missing_devices,
            extra: m.extra_devices,
            stale: m.stale_devices,
        }),
        None => Ok(response),
    }
}
