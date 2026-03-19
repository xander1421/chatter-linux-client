//! Notification gRPC client.
//!
//! Client for notification service operations including notification management,
//! preferences, and real-time notification streaming.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    notification_service_client::NotificationServiceClient as ProtoNotificationServiceClient,
    CreateBatchNotificationsRequest, CreateBatchNotificationsResponse, CreateNotificationRequest,
    CreateNotificationResponse, DeleteNotificationRequest, DeleteNotificationResponse,
    GetNotificationRequest, GetNotificationResponse, GetPreferencesRequest, GetPreferencesResponse,
    GetUnreadCountRequest, GetUnreadCountResponse, ListNotificationsRequest,
    ListNotificationsResponse, MarkAllAsReadRequest, MarkAllAsReadResponse, MarkAsReadRequest,
    MarkAsReadResponse, NotificationPreferences, NotificationPriority, NotificationType,
    StreamNotificationsRequest, StreamNotificationsResponse, UpdatePreferencesRequest,
    UpdatePreferencesResponse,
};

/// Notification service client wrapper.
///
/// Wraps the generated NotificationServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut notification = NotificationClient::new(pool.channel(), pool.auth_clone());
/// let unread = notification.get_unread_count(None).await?;
/// ```
pub struct NotificationClient {
    inner: ProtoNotificationServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl NotificationClient {
    /// Create a new NotificationClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: ProtoNotificationServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== CRUD RPCs ====================

    /// Create a new notification.
    ///
    /// # Arguments
    ///
    /// * `user_id` - Target user ID
    /// * `notification_type` - Type of notification
    /// * `priority` - Notification priority
    /// * `title` - Notification title
    /// * `body` - Notification body
    /// * `metadata` - Optional metadata (JSON)
    /// * `channel_id` - Optional associated channel ID
    /// * `message_id` - Optional associated message ID
    /// * `actor_id` - Optional actor (user who triggered) ID
    #[allow(clippy::too_many_arguments)]
    pub async fn create_notification(
        &mut self,
        user_id: &str,
        notification_type: NotificationType,
        priority: NotificationPriority,
        title: &str,
        body: &str,
        metadata: Option<&str>,
        channel_id: Option<&str>,
        message_id: Option<&str>,
        actor_id: Option<&str>,
    ) -> Result<CreateNotificationResponse, NetworkError> {
        let request = CreateNotificationRequest {
            user_id: user_id.to_string(),
            r#type: notification_type.into(),
            priority: priority.into(),
            title: title.to_string(),
            body: body.to_string(),
            metadata: metadata.unwrap_or("").to_string(),
            channel_id: channel_id.unwrap_or("").to_string(),
            message_id: message_id.unwrap_or("").to_string(),
            actor_id: actor_id.unwrap_or("").to_string(),
            expires_at: 0,
            action_url: String::new(),
        };

        self.inner
            .create_notification(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Create multiple notifications in batch.
    ///
    /// # Arguments
    ///
    /// * `notifications` - List of notification creation requests
    pub async fn create_batch_notifications(
        &mut self,
        notifications: Vec<CreateNotificationRequest>,
    ) -> Result<CreateBatchNotificationsResponse, NetworkError> {
        let request = CreateBatchNotificationsRequest { notifications };

        self.inner
            .create_batch_notifications(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get a specific notification by ID.
    ///
    /// # Arguments
    ///
    /// * `notification_id` - The notification ID to retrieve
    pub async fn get_notification(
        &mut self,
        notification_id: &str,
    ) -> Result<GetNotificationResponse, NetworkError> {
        let request = GetNotificationRequest {
            id: notification_id.to_string(),
        };

        self.inner
            .get_notification(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List notifications for a user with pagination.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID to list notifications for (uses authenticated user if None)
    /// * `unread_only` - Filter to only unread notifications
    /// * `types` - Optional filter by notification types
    /// * `limit` - Maximum number of notifications to return
    /// * `offset` - Pagination offset
    pub async fn list_notifications(
        &mut self,
        user_id: Option<&str>,
        unread_only: bool,
        types: Vec<NotificationType>,
        limit: i32,
        offset: i32,
    ) -> Result<ListNotificationsResponse, NetworkError> {
        let request = ListNotificationsRequest {
            user_id: user_id.unwrap_or("").to_string(),
            unread_only,
            types: types.into_iter().map(|t| t.into()).collect(),
            limit,
            offset,
        };

        self.inner
            .list_notifications(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete a notification.
    ///
    /// # Arguments
    ///
    /// * `notification_id` - The notification ID to delete
    pub async fn delete_notification(
        &mut self,
        notification_id: &str,
    ) -> Result<DeleteNotificationResponse, NetworkError> {
        let request = DeleteNotificationRequest {
            id: notification_id.to_string(),
        };

        self.inner
            .delete_notification(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Status Management RPCs ====================

    /// Mark specific notifications as read.
    ///
    /// # Arguments
    ///
    /// * `notification_ids` - List of notification IDs to mark as read
    pub async fn mark_as_read(
        &mut self,
        notification_ids: Vec<String>,
    ) -> Result<MarkAsReadResponse, NetworkError> {
        let request = MarkAsReadRequest { notification_ids };

        self.inner
            .mark_as_read(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Mark all notifications as read for a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID (uses authenticated user if None)
    /// * `types` - Optional filter to only mark specific notification types
    pub async fn mark_all_as_read(
        &mut self,
        user_id: Option<&str>,
        types: Vec<NotificationType>,
    ) -> Result<MarkAllAsReadResponse, NetworkError> {
        let request = MarkAllAsReadRequest {
            user_id: user_id.unwrap_or("").to_string(),
            types: types.into_iter().map(|t| t.into()).collect(),
        };

        self.inner
            .mark_all_as_read(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get unread notification count.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID (uses authenticated user if None)
    /// * `types` - Optional filter to count specific notification types
    pub async fn get_unread_count(
        &mut self,
        user_id: Option<&str>,
        types: Vec<NotificationType>,
    ) -> Result<GetUnreadCountResponse, NetworkError> {
        let request = GetUnreadCountRequest {
            user_id: user_id.unwrap_or("").to_string(),
            types: types.into_iter().map(|t| t.into()).collect(),
        };

        self.inner
            .get_unread_count(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Preferences RPCs ====================

    /// Get notification preferences for a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID (uses authenticated user if None)
    pub async fn get_preferences(
        &mut self,
        user_id: Option<&str>,
    ) -> Result<GetPreferencesResponse, NetworkError> {
        let request = GetPreferencesRequest {
            user_id: user_id.unwrap_or("").to_string(),
        };

        self.inner
            .get_preferences(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Update notification preferences.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID (uses authenticated user if None)
    /// * `preferences` - The new notification preferences
    pub async fn update_preferences(
        &mut self,
        user_id: Option<&str>,
        preferences: NotificationPreferences,
    ) -> Result<UpdatePreferencesResponse, NetworkError> {
        let request = UpdatePreferencesRequest {
            user_id: user_id.unwrap_or("").to_string(),
            preferences: Some(preferences),
        };

        self.inner
            .update_preferences(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Streaming RPCs ====================

    /// Stream notifications in real-time.
    ///
    /// Long-lived stream for real-time notification updates.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID (uses authenticated user if None)
    /// * `types` - Optional filter by notification types
    pub async fn stream_notifications(
        &mut self,
        user_id: Option<&str>,
        types: Vec<NotificationType>,
    ) -> Result<impl Stream<Item = Result<StreamNotificationsResponse, NetworkError>>, NetworkError>
    {
        let request = StreamNotificationsRequest {
            user_id: user_id.unwrap_or("").to_string(),
            types: types.into_iter().map(|t| t.into()).collect(),
        };

        let response = self
            .inner
            .stream_notifications(request)
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
    fn test_notification_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = NotificationClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_create_notification_request_construction() {
        // Verify CreateNotificationRequest can be constructed
        let request = CreateNotificationRequest {
            user_id: "user-123".to_string(),
            r#type: NotificationType::Message as i32,
            priority: NotificationPriority::Normal as i32,
            title: "New Message".to_string(),
            body: "You have a new message".to_string(),
            metadata: "{}".to_string(),
            channel_id: "channel-456".to_string(),
            message_id: "msg-789".to_string(),
            actor_id: "actor-111".to_string(),
            expires_at: 0,
            action_url: String::new(),
        };

        assert_eq!(request.user_id, "user-123");
        assert_eq!(request.r#type, NotificationType::Message as i32);
        assert_eq!(request.priority, NotificationPriority::Normal as i32);
        assert_eq!(request.title, "New Message");
    }

    #[test]
    fn test_list_notifications_request_construction() {
        // Verify ListNotificationsRequest can be constructed with filters
        let request = ListNotificationsRequest {
            user_id: "user-123".to_string(),
            unread_only: true,
            types: vec![NotificationType::Message as i32, NotificationType::Mention as i32],
            limit: 20,
            offset: 0,
        };

        assert_eq!(request.user_id, "user-123");
        assert!(request.unread_only);
        assert_eq!(request.types.len(), 2);
        assert_eq!(request.limit, 20);
    }

    #[test]
    fn test_mark_as_read_request_construction() {
        // Verify MarkAsReadRequest can be constructed with notification_ids
        let request = MarkAsReadRequest {
            notification_ids: vec!["notif-1".to_string(), "notif-2".to_string(), "notif-3".to_string()],
        };

        assert_eq!(request.notification_ids.len(), 3);
        assert_eq!(request.notification_ids[0], "notif-1");
    }

    #[test]
    fn test_stream_notifications_request_construction() {
        // Verify StreamNotificationsRequest can be constructed
        let request = StreamNotificationsRequest {
            user_id: "user-123".to_string(),
            types: vec![NotificationType::Message as i32],
        };

        assert_eq!(request.user_id, "user-123");
        assert_eq!(request.types.len(), 1);
    }

    #[test]
    fn test_notification_type_values() {
        // Verify NotificationType enum is accessible and has expected values
        assert_eq!(NotificationType::Unspecified as i32, 0);
        assert_eq!(NotificationType::Message as i32, 1);
        assert_eq!(NotificationType::Mention as i32, 2);
        assert_eq!(NotificationType::Reply as i32, 3);
        assert_eq!(NotificationType::Reaction as i32, 4);
    }

    #[test]
    fn test_notification_priority_values() {
        // Verify NotificationPriority enum is accessible and has expected values
        assert_eq!(NotificationPriority::Unspecified as i32, 0);
        assert_eq!(NotificationPriority::Low as i32, 1);
        assert_eq!(NotificationPriority::Normal as i32, 2);
        assert_eq!(NotificationPriority::High as i32, 3);
    }

    #[test]
    fn test_notification_preferences_construction() {
        // Verify NotificationPreferences can be constructed
        let prefs = NotificationPreferences {
            user_id: "user-123".to_string(),
            enabled: true,
            push_enabled: true,
            email_enabled: false,
            desktop_enabled: true,
            type_preferences: std::collections::HashMap::new(),
            quiet_hours_start: "22:00".to_string(),
            quiet_hours_end: "08:00".to_string(),
            timezone: "America/New_York".to_string(),
            updated_at: 0,
        };

        assert_eq!(prefs.user_id, "user-123");
        assert!(prefs.enabled);
        assert!(prefs.push_enabled);
        assert!(!prefs.email_enabled);
        assert!(prefs.desktop_enabled);
    }
}
