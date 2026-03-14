//! Typing indicator service.
//!
//! Orchestrates typing indicator operations using existing network clients
//! and streaming aggregators. Provides send operations, local queries,
//! and subscription APIs for real-time typing state.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::ids::UserId;
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::message_client::MessageClient;
use crate::network::streaming::typing_stream::{TypingAggregator, TypingStateChange};
use crate::proto::chat_v1::TypingIndicator;
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Typing service errors.
#[derive(Debug, thiserror::Error)]
pub enum TypingError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl From<CurrentUserError> for TypingError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => TypingError::Storage(e),
            CurrentUserError::NotLoggedIn => TypingError::NotLoggedIn,
        }
    }
}

/// Typing indicator service.
///
/// Orchestrates typing indicator sending and provides local cache queries.
/// Uses the TypingAggregator for caching and broadcasting updates.
///
/// # Example
///
/// ```ignore
/// let service = TypingService::new(settings_repo, typing_aggregator);
///
/// // Start typing in a channel
/// service.start_typing(&mut message_client, "channel-123").await?;
///
/// // Set typing indicator with auto-clear
/// service.set_typing_indicator("user-123", "channel-123", TypingIndicatorType::DirectMessage);
///
/// // Check who's typing (local cache, no network)
/// let typing_users = service.get_typing_users("channel-123");
/// ```
pub struct TypingService {
    settings_repo: Arc<SettingsRepository>,
    typing_aggregator: Arc<TypingAggregator>,
}

impl TypingService {
    /// Create a new TypingService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        typing_aggregator: Arc<TypingAggregator>,
    ) -> Self {
        Self {
            settings_repo,
            typing_aggregator,
        }
    }

    // ========================================================================
    // Server Operations
    // ========================================================================

    /// Start typing indicator in a channel.
    ///
    /// Sends a typing indicator to the server indicating the current user
    /// has started typing.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `channel_id` - Channel where user is typing
    pub async fn start_typing(
        &self,
        client: &mut MessageClient,
        channel_id: &str,
    ) -> Result<(), TypingError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Send typing indicator to server
        client.send_typing_indicator(channel_id, true).await?;

        Ok(())
    }

    /// Stop typing indicator in a channel.
    ///
    /// Sends a typing indicator to the server indicating the current user
    /// has stopped typing.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `channel_id` - Channel where user stopped typing
    pub async fn stop_typing(
        &self,
        client: &mut MessageClient,
        channel_id: &str,
    ) -> Result<(), TypingError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Send stop typing indicator to server
        client.send_typing_indicator(channel_id, false).await?;

        Ok(())
    }

    // ========================================================================
    // Stream Handling
    // ========================================================================

    /// Process an incoming typing indicator event.
    ///
    /// Delegates to the typing aggregator to update local state and
    /// broadcast changes to subscribers.
    ///
    /// # Arguments
    ///
    /// * `indicator` - The typing indicator event from the stream
    pub fn process_typing_event(&self, indicator: TypingIndicator) {
        self.typing_aggregator.process(indicator);
    }

    // ========================================================================
    // Subscription
    // ========================================================================

    /// Subscribe to typing state change events.
    ///
    /// Returns a broadcast receiver for typing state changes. Use this to
    /// react to typing updates in the UI layer.
    pub fn subscribe(&self) -> broadcast::Receiver<TypingStateChange> {
        self.typing_aggregator.on_typing_change()
    }

    // ========================================================================
    // Local Query Methods (no network, no auth check needed)
    // ========================================================================

    /// Get users currently typing in a channel.
    ///
    /// Returns a list of user IDs from the local cache.
    /// Automatically filters out expired entries.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check
    pub fn get_typing_users(&self, channel_id: &str) -> Vec<UserId> {
        self.typing_aggregator
            .get_typing_users(channel_id)
            .into_iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    /// Check if anyone is typing in a channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check
    pub fn is_anyone_typing(&self, channel_id: &str) -> bool {
        self.typing_aggregator.is_anyone_typing(channel_id)
    }

    // ========================================================================
    // Cache Management
    // ========================================================================

    /// Clear all typing state.
    ///
    /// Call when disconnecting or logging out.
    pub fn clear_typing(&self) {
        self.typing_aggregator.clear();
    }

    /// Clean up expired typing entries.
    ///
    /// Call periodically (e.g., every 5 seconds) to remove stale entries.
    /// Returns the number of entries removed.
    pub fn cleanup_expired(&self) -> usize {
        self.typing_aggregator.cleanup_expired()
    }
    
    /// Start background cleanup task for expired typing indicators.
    ///
    /// This prevents typing indicators from getting stuck by periodically
    /// removing expired entries. Returns a JoinHandle for the background task.
    pub fn start_cleanup_task(&self) -> tokio::task::JoinHandle<()> {
        crate::services::typing_cleanup::start_typing_cleanup(self.typing_aggregator.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::tempdir;

    /// Create a test TypingService with a proper temporary database.
    fn create_test_service() -> (TypingService, Arc<TypingAggregator>, tempfile::TempDir) {
        let aggregator = Arc::new(TypingAggregator::new());

        // Create a unique temp directory
        let temp_dir = tempdir().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");

        let db = Database::open(&db_path, "test-passphrase")
            .expect("Failed to open test database");

        // Create minimal schema
        {
            let conn = db.connection().expect("Failed to get connection");
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                "#,
            )
            .expect("Failed to create tables");
        }

        let settings_repo = Arc::new(SettingsRepository::new(Arc::new(db)));

        let service = TypingService::new(settings_repo, Arc::clone(&aggregator));
        (service, aggregator, temp_dir)
    }

    #[test]
    fn test_typing_error_display() {
        let err = TypingError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));
    }

    #[test]
    fn test_typing_service_construction() {
        // Verify the TypingService struct type signatures compile correctly.
        fn _verify_types(
            settings_repo: Arc<SettingsRepository>,
            typing_aggregator: Arc<TypingAggregator>,
        ) {
            let _service = TypingService::new(settings_repo, typing_aggregator);
        }
    }

    #[test]
    fn test_get_typing_users_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown channel should return empty vec
        let users = service.get_typing_users("unknown-channel");
        assert!(users.is_empty());
    }

    #[test]
    fn test_is_anyone_typing_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown channel should return false
        assert!(!service.is_anyone_typing("unknown-channel"));
    }

    #[test]
    fn test_process_typing_event() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Process a typing indicator
        service.process_typing_event(TypingIndicator {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: true,
        });

        // Should now have a typing user
        assert!(service.is_anyone_typing("channel-1"));

        let users = service.get_typing_users("channel-1");
        assert_eq!(users.len(), 1);
    }

    #[test]
    fn test_process_typing_stopped() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Start typing
        service.process_typing_event(TypingIndicator {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: true,
        });

        // Stop typing
        service.process_typing_event(TypingIndicator {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: false,
        });

        // Should no longer have typing users
        assert!(!service.is_anyone_typing("channel-1"));
    }

    #[test]
    fn test_clear_typing() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Add some typing state
        service.process_typing_event(TypingIndicator {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: true,
        });

        assert!(service.is_anyone_typing("channel-1"));

        // Clear all
        service.clear_typing();

        // Should be empty
        assert!(!service.is_anyone_typing("channel-1"));
    }

    #[test]
    fn test_subscribe_returns_receiver() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Should be able to subscribe
        let _rx = service.subscribe();
    }

    #[tokio::test]
    async fn test_subscribe_receives_events() {
        let (service, _aggregator, _temp_dir) = create_test_service();
        let mut rx = service.subscribe();

        // Process an event
        service.process_typing_event(TypingIndicator {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: true,
        });

        // Should receive the change
        let change = rx.try_recv().unwrap();
        assert_eq!(change.channel_id, "channel-1");
        assert_eq!(change.typing_users.len(), 1);
    }
}
