//! Saved messages gRPC client.
//!
//! Client for saved messages service operations including saving messages,
//! listing, deleting, pinning/unpinning, and listing pinned messages.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::saved_messages_v1::{
    saved_messages_service_client::SavedMessagesServiceClient, DeleteSavedMessageRequest,
    ListPinnedSavedMessagesRequest, ListSavedMessagesRequest, ListSavedMessagesResponse,
    PinSavedMessageRequest, PinnedSortBy, SaveMessageRequest, SavedMessage, SavedMessageContent,
    SavedMessageSource, SortDirection, UnpinSavedMessageRequest,
};

/// Saved messages service client wrapper.
///
/// Wraps the generated SavedMessagesServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut client = SavedMessagesClient::new(pool.channel(), pool.auth_clone());
/// let saved = client.save_message(content, None, "req-123").await?;
/// ```
pub struct SavedMessagesClient {
    inner: SavedMessagesServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl SavedMessagesClient {
    /// Create a new SavedMessagesClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: SavedMessagesServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Core Message RPCs ====================

    /// Save a message to the personal vault.
    ///
    /// Creates a new saved message with the provided content and optional source.
    ///
    /// # Arguments
    ///
    /// * `content` - The content to save (encrypted)
    /// * `source` - Optional source information (where it came from)
    /// * `client_request_id` - Idempotency key (UUID)
    pub async fn save_message(
        &mut self,
        content: SavedMessageContent,
        source: Option<SavedMessageSource>,
        client_request_id: &str,
    ) -> Result<SavedMessage, NetworkError> {
        let request = SaveMessageRequest {
            content: Some(content),
            client_request_id: client_request_id.to_string(),
            source,
            client_timestamp: chrono::Utc::now().timestamp_millis() as u64,
        };

        let response = self
            .inner
            .save_message(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        response
            .into_inner()
            .message
            .ok_or_else(|| NetworkError::Unknown("missing saved message in response".to_string()))
    }

    /// List saved messages with pagination.
    ///
    /// Returns saved messages sorted by the specified direction.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of messages to return
    /// * `page_token` - Cursor token for pagination
    /// * `sort_direction` - Sort direction (newest or oldest first)
    /// * `from_timestamp` - Optional start of time range filter (inclusive, Unix ms)
    /// * `to_timestamp` - Optional end of time range filter (inclusive, Unix ms)
    pub async fn list_messages(
        &mut self,
        limit: i32,
        page_token: &str,
        sort_direction: SortDirection,
        from_timestamp: Option<u64>,
        to_timestamp: Option<u64>,
    ) -> Result<ListSavedMessagesResponse, NetworkError> {
        let request = ListSavedMessagesRequest {
            limit,
            page_token: page_token.to_string(),
            sort_direction: sort_direction as i32,
            from_timestamp: from_timestamp.unwrap_or(0),
            to_timestamp: to_timestamp.unwrap_or(0),
        };

        self.inner
            .list_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete a saved message.
    ///
    /// Permanently removes a message from the personal vault.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to delete (ULID)
    /// * `request_id` - Idempotency key (UUID)
    pub async fn delete_message(
        &mut self,
        message_id: &str,
        request_id: &str,
    ) -> Result<(), NetworkError> {
        let request = DeleteSavedMessageRequest {
            message_id: message_id.to_string(),
            request_id: request_id.to_string(),
        };

        self.inner
            .delete_message(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Pin RPCs ====================

    /// Pin a saved message.
    ///
    /// Pins a message for quick access. Returns the updated message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to pin (ULID)
    /// * `request_id` - Idempotency key (UUID)
    pub async fn pin_message(
        &mut self,
        message_id: &str,
        request_id: &str,
    ) -> Result<SavedMessage, NetworkError> {
        let request = PinSavedMessageRequest {
            message_id: message_id.to_string(),
            request_id: request_id.to_string(),
        };

        self.inner
            .pin_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Unpin a saved message.
    ///
    /// Removes the pin from a message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to unpin (ULID)
    /// * `request_id` - Idempotency key (UUID)
    pub async fn unpin_message(
        &mut self,
        message_id: &str,
        request_id: &str,
    ) -> Result<(), NetworkError> {
        let request = UnpinSavedMessageRequest {
            message_id: message_id.to_string(),
            request_id: request_id.to_string(),
        };

        self.inner
            .unpin_message(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List pinned saved messages.
    ///
    /// Returns only pinned messages, sorted by pin time or save time.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of messages to return
    /// * `page_token` - Cursor token for pagination
    /// * `sort_by` - Sort by pin time or creation time
    /// * `sort_direction` - Sort direction (newest or oldest first)
    pub async fn list_pinned_messages(
        &mut self,
        limit: i32,
        page_token: &str,
        sort_by: PinnedSortBy,
        sort_direction: SortDirection,
    ) -> Result<ListSavedMessagesResponse, NetworkError> {
        let request = ListPinnedSavedMessagesRequest {
            limit,
            page_token: page_token.to_string(),
            sort_by: sort_by as i32,
            sort_direction: sort_direction as i32,
        };

        self.inner
            .list_pinned_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}

#[cfg(test)]
mod tests {
    use crate::proto::saved_messages_v1::{
        DeleteSavedMessageRequest, ListPinnedSavedMessagesRequest, ListSavedMessagesRequest,
        PinSavedMessageRequest, PinnedSortBy, SaveMessageRequest, SortDirection,
        UnpinSavedMessageRequest,
    };

    #[test]
    fn test_saved_messages_client_construction_types() {
        // Verify the type signatures compile correctly.
        use super::*;
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = SavedMessagesClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_save_message_request_construction() {
        let request = SaveMessageRequest {
            content: None,
            client_request_id: "req-123".to_string(),
            source: None,
            client_timestamp: 1234567890000,
        };
        assert_eq!(request.client_request_id, "req-123");
        assert_eq!(request.client_timestamp, 1234567890000);
    }

    #[test]
    fn test_list_saved_messages_request_construction() {
        let request = ListSavedMessagesRequest {
            limit: 50,
            page_token: "".to_string(),
            sort_direction: SortDirection::NewestFirst as i32,
            from_timestamp: 0,
            to_timestamp: 0,
        };
        assert_eq!(request.limit, 50);
        assert_eq!(request.sort_direction, SortDirection::NewestFirst as i32);
    }

    #[test]
    fn test_delete_saved_message_request_construction() {
        let request = DeleteSavedMessageRequest {
            message_id: "msg-123".to_string(),
            request_id: "req-456".to_string(),
        };
        assert_eq!(request.message_id, "msg-123");
        assert_eq!(request.request_id, "req-456");
    }

    #[test]
    fn test_pin_saved_message_request_construction() {
        let request = PinSavedMessageRequest {
            message_id: "msg-123".to_string(),
            request_id: "req-456".to_string(),
        };
        assert_eq!(request.message_id, "msg-123");
    }

    #[test]
    fn test_unpin_saved_message_request_construction() {
        let request = UnpinSavedMessageRequest {
            message_id: "msg-123".to_string(),
            request_id: "req-456".to_string(),
        };
        assert_eq!(request.message_id, "msg-123");
    }

    #[test]
    fn test_list_pinned_saved_messages_request_construction() {
        let request = ListPinnedSavedMessagesRequest {
            limit: 25,
            page_token: "token".to_string(),
            sort_by: PinnedSortBy::PinnedAt as i32,
            sort_direction: SortDirection::NewestFirst as i32,
        };
        assert_eq!(request.limit, 25);
        assert_eq!(request.sort_by, PinnedSortBy::PinnedAt as i32);
    }

    #[test]
    fn test_sort_direction_values() {
        assert_eq!(SortDirection::Unspecified as i32, 0);
        assert_eq!(SortDirection::NewestFirst as i32, 1);
        assert_eq!(SortDirection::OldestFirst as i32, 2);
    }

    #[test]
    fn test_pinned_sort_by_values() {
        assert_eq!(PinnedSortBy::Unspecified as i32, 0);
        assert_eq!(PinnedSortBy::PinnedAt as i32, 1);
        assert_eq!(PinnedSortBy::CreatedAt as i32, 2);
    }
}
