//! Message gRPC client.
//!
//! Client for message service operations including messaging, reactions,
//! read receipts, typing indicators, pins, search, and streaming.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    message_service_client::MessageServiceClient, AcknowledgeMessagesRequest,
    AcknowledgeMessagesResponse, DeleteMessageRequest, EditMessageRequest, GetDeviceMessagesRequest,
    GetDeviceMessagesResponse, GetMessageReactionsRequest, GetMessageReactionsResponse,
    GetMessagesRequest, GetMessagesResponse, GetReadReceiptsRequest, GetReadReceiptsResponse,
    ListPinnedMessagesResponse, Message, MessageEvent, MessageSearchRequest, MessageSearchResponse,
    MessageSortDirection, MessageUpdate, ReactionRequest, SealedSenderMessage,
    SendMessageRequest, SendReadReceiptRequest, SendSealedMessageRequest, SendSealedMessageResponse,
    SendTypingIndicatorRequest, StreamGlobalEventsRequest, StreamMessageEventsRequest,
    StreamMessagesRequest,
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
/// let message = client.send_message("channel-123", content, None, false, None, false, None, vec![], false, vec![]).await?;
/// ```
pub struct MessageClient {
    inner: MessageServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl MessageClient {
    /// Create a new MessageClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
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
    /// * `encrypt` - Whether to request encryption
    /// * `crypto_options` - Optional encryption preferences
    /// * `mentioned_integrations` - Integration IDs mentioned in message
    /// * `targets_ai_bot` - Whether message targets AI bot
    /// * `mentioned_file_ids` - File IDs mentioned in message
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &mut self,
        channel_id: &str,
        content: crate::proto::chat_v1::message::Content,
        parent_id: Option<&str>,
        ephemeral: bool,
        ephemeral_ttl: Option<prost_types::Duration>,
        encrypt: bool,
        crypto_options: Option<crate::proto::chat_v1::CryptoOptions>,
        mentioned_integrations: Vec<String>,
        targets_ai_bot: bool,
        mentioned_file_ids: Vec<String>,
    ) -> Result<Message, NetworkError> {
        let request = SendMessageRequest {
            channel_id: channel_id.to_string(),
            parent_id: parent_id.unwrap_or("").to_string(),
            content: Some(content),
            ephemeral,
            ephemeral_ttl,
            client_request_id: uuid::Uuid::new_v4().to_string(),
            encrypt_message: encrypt,
            crypto_options,
            mentioned_integrations,
            targets_ai_bot,
            mentioned_file_ids,
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
    pub async fn delete_message(
        &mut self,
        message_id: &str,
        channel_id: &str,
    ) -> Result<(), NetworkError> {
        let request = DeleteMessageRequest {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
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
        };

        self.inner
            .get_messages(request)
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
    pub async fn stream_messages(
        &mut self,
        channel_id: &str,
        resume_from: Option<&str>,
    ) -> Result<impl Stream<Item = Result<Message, NetworkError>>, NetworkError> {
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

    /// Send a sealed sender message (anonymous to server).
    ///
    /// This endpoint uses UAK (Unidentified Access Key) authentication
    /// instead of regular auth, hiding sender identity from the server.
    ///
    /// # Arguments
    ///
    /// * `destination_user_id` - Recipient user ID
    /// * `destination_device_id` - Recipient device ID (0 for all devices)
    /// * `sealed_message` - The sealed sender envelope
    pub async fn send_sealed_message(
        &mut self,
        destination_user_id: &str,
        destination_device_id: u32,
        sealed_message: SealedSenderMessage,
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        let request = SendSealedMessageRequest {
            destination_user_id: destination_user_id.to_string(),
            destination_device_id,
            sealed_message: Some(sealed_message),
            client_request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .send_sealed_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
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

    // ==================== Reaction RPCs ====================

    /// Add a reaction to a message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - Message to react to
    /// * `channel_id` - Channel containing the message
    /// * `emoji` - Emoji reaction (Unicode or custom format)
    pub async fn add_reaction(
        &mut self,
        message_id: &str,
        channel_id: &str,
        emoji: &str,
    ) -> Result<Message, NetworkError> {
        let request = ReactionRequest {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            emoji: emoji.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .add_reaction(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Remove a reaction from a message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - Message to remove reaction from
    /// * `channel_id` - Channel containing the message
    /// * `emoji` - Emoji reaction to remove
    pub async fn remove_reaction(
        &mut self,
        message_id: &str,
        channel_id: &str,
        emoji: &str,
    ) -> Result<Message, NetworkError> {
        let request = ReactionRequest {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            emoji: emoji.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .remove_reaction(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get reactions for a message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - Message to get reactions for
    /// * `channel_id` - Channel containing the message
    /// * `limit` - Max reactions to return
    /// * `offset` - Pagination offset
    /// * `emoji_filter` - Optional filter by specific emoji
    pub async fn get_message_reactions(
        &mut self,
        message_id: &str,
        channel_id: &str,
        limit: i32,
        offset: i32,
        emoji_filter: Option<&str>,
    ) -> Result<GetMessageReactionsResponse, NetworkError> {
        let request = GetMessageReactionsRequest {
            message_id: message_id.to_string(),
            channel_id: channel_id.to_string(),
            limit,
            offset,
            emoji_filter: emoji_filter.unwrap_or("").to_string(),
        };

        self.inner
            .get_message_reactions(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Read Receipts & Typing ====================

    /// Send a read receipt for a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel being read
    /// * `last_read_message_id` - ID of the last message read
    pub async fn send_read_receipt(
        &mut self,
        channel_id: &str,
        last_read_message_id: &str,
    ) -> Result<(), NetworkError> {
        let request = SendReadReceiptRequest {
            channel_id: channel_id.to_string(),
            last_read_message_id: last_read_message_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .send_read_receipt(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get read receipts for a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to get receipts for
    /// * `thread_root_id` - Optional thread filter
    /// * `limit` - Max receipts to return
    pub async fn get_read_receipts(
        &mut self,
        channel_id: &str,
        thread_root_id: Option<&str>,
        limit: i32,
    ) -> Result<GetReadReceiptsResponse, NetworkError> {
        let request = GetReadReceiptsRequest {
            channel_id: channel_id.to_string(),
            thread_root_id: thread_root_id.unwrap_or("").to_string(),
            limit,
        };

        self.inner
            .get_read_receipts(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Send a typing indicator.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel where user is typing
    /// * `is_typing` - True if user is typing, false if stopped
    pub async fn send_typing_indicator(
        &mut self,
        channel_id: &str,
        is_typing: bool,
    ) -> Result<(), NetworkError> {
        let request = SendTypingIndicatorRequest {
            channel_id: channel_id.to_string(),
            is_typing,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .send_typing_indicator(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
