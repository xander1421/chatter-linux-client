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
}
