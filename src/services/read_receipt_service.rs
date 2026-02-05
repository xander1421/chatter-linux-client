//! Read receipt service.
//!
//! Orchestrates read receipt operations using existing network clients
//! and streaming aggregators. Provides send operations, local queries,
//! and subscription APIs for read receipt tracking.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::ids::UserId;
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::message_client::MessageClient;
use crate::network::streaming::read_receipt_stream::{ReadReceiptAggregator, ReadState};
use crate::proto::chat_v1::ReadReceipt;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Read receipt service errors.
#[derive(Debug, thiserror::Error)]
pub enum ReadReceiptError {
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

/// Read receipt service.
///
/// Orchestrates read receipt sending and provides local cache queries.
/// Uses the ReadReceiptAggregator for caching and broadcasting updates.
///
/// # Example
///
/// ```ignore
/// let service = ReadReceiptService::new(settings_repo, receipt_aggregator);
///
/// // Mark a channel as read up to a message
/// service.mark_as_read(&mut message_client, "channel-123", "msg-456").await?;
///
/// // Check last read for a user (local cache, no network)
/// if let Some(msg_id) = service.get_last_read("channel-123", &user_id) {
///     println!("Last read: {}", msg_id);
/// }
/// ```
pub struct ReadReceiptService {
    settings_repo: Arc<SettingsRepository>,
    receipt_aggregator: Arc<ReadReceiptAggregator>,
}

impl ReadReceiptService {
    /// Create a new ReadReceiptService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        receipt_aggregator: Arc<ReadReceiptAggregator>,
    ) -> Self {
        Self {
            settings_repo,
            receipt_aggregator,
        }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<UserId, ReadReceiptError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(ReadReceiptError::Storage)?
            .ok_or(ReadReceiptError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| ReadReceiptError::NotLoggedIn)
    }

    // ========================================================================
    // Server Operations
    // ========================================================================

    /// Mark a channel as read up to a specific message.
    ///
    /// Sends a read receipt to the server and updates the local cache.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `channel_id` - Channel to mark as read
    /// * `last_message_id` - ID of the last read message
    pub async fn mark_as_read(
        &self,
        client: &mut MessageClient,
        channel_id: &str,
        last_message_id: &str,
    ) -> Result<(), ReadReceiptError> {
        // Check logged in and get user ID
        let current_user = self.current_user_id()?;

        // Send read receipt to server
        client.send_read_receipt(channel_id, last_message_id).await?;

        // Update local cache with current user's receipt
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        self.process_read_receipt(ReadReceipt {
            channel_id: channel_id.to_string(),
            user_id: current_user.to_string(),
            last_read_id: last_message_id.to_string(),
            timestamp: now_ms,
        });

        Ok(())
    }

    /// Get read receipts for a channel from the server.
    ///
    /// Fetches read receipts from the server. Use for initial sync or refresh.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `channel_id` - Channel to get receipts for
    /// * `limit` - Maximum receipts to return
    pub async fn get_receipts(
        &self,
        client: &mut MessageClient,
        channel_id: &str,
        limit: i32,
    ) -> Result<Vec<ReadReceipt>, ReadReceiptError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server
        let response = client.get_read_receipts(channel_id, None, limit).await?;

        Ok(response.read_receipts)
    }

    // ========================================================================
    // Stream Handling
    // ========================================================================

    /// Process an incoming read receipt event.
    ///
    /// Delegates to the receipt aggregator to update local state and
    /// broadcast changes to subscribers.
    ///
    /// # Arguments
    ///
    /// * `receipt` - The read receipt event from the stream
    pub fn process_read_receipt(&self, receipt: ReadReceipt) {
        self.receipt_aggregator.process(receipt);
    }

    // ========================================================================
    // Subscription
    // ========================================================================

    /// Subscribe to read receipt events.
    ///
    /// Returns a broadcast receiver for read receipt events. Use this to
    /// react to read receipt updates in the UI layer.
    pub fn subscribe(&self) -> broadcast::Receiver<ReadReceipt> {
        self.receipt_aggregator.on_read_receipt()
    }

    // ========================================================================
    // Local Query Methods (no network, no auth check needed)
    // ========================================================================

    /// Get the last read message ID for a user in a channel.
    ///
    /// Returns from local cache. Returns None if not found.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check
    /// * `user_id` - User to check
    pub fn get_last_read(&self, channel_id: &str, user_id: &UserId) -> Option<String> {
        self.receipt_aggregator.get_last_read(channel_id, &user_id.to_string())
    }

    /// Get all read states for a channel.
    ///
    /// Returns a map of user_id -> read_state from local cache.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check
    pub fn get_channel_read_states(&self, channel_id: &str) -> HashMap<UserId, ReadState> {
        self.receipt_aggregator
            .get_channel_read_states(channel_id)
            .into_iter()
            .filter_map(|(user_id_str, state)| {
                user_id_str.parse::<UserId>().ok().map(|id| (id, state))
            })
            .collect()
    }

    /// Get users who have read up to a specific message.
    ///
    /// Returns users whose last_read_id matches the given message.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to check
    /// * `message_id` - Message ID to check
    pub fn get_readers_of_message(&self, channel_id: &str, message_id: &str) -> Vec<UserId> {
        self.receipt_aggregator
            .get_readers_of_message(channel_id, message_id)
            .into_iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    // ========================================================================
    // Cache Management
    // ========================================================================

    /// Clear read states for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to clear
    pub fn clear_channel(&self, channel_id: &str) {
        self.receipt_aggregator.clear_channel(channel_id);
    }

    /// Clear all read states.
    ///
    /// Call when disconnecting or logging out.
    pub fn clear_all(&self) {
        self.receipt_aggregator.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::tempdir;

    /// Create a test ReadReceiptService with a proper temporary database.
    fn create_test_service() -> (ReadReceiptService, Arc<ReadReceiptAggregator>, tempfile::TempDir) {
        let aggregator = Arc::new(ReadReceiptAggregator::new());

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

        let service = ReadReceiptService::new(settings_repo, Arc::clone(&aggregator));
        (service, aggregator, temp_dir)
    }

    fn make_receipt(channel_id: &str, user_id: &str, message_id: &str, timestamp: u64) -> ReadReceipt {
        ReadReceipt {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            last_read_id: message_id.to_string(),
            timestamp,
        }
    }

    #[test]
    fn test_read_receipt_error_display() {
        let err = ReadReceiptError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));
    }

    #[test]
    fn test_read_receipt_service_construction() {
        // Verify the ReadReceiptService struct type signatures compile correctly.
        fn _verify_types(
            settings_repo: Arc<SettingsRepository>,
            receipt_aggregator: Arc<ReadReceiptAggregator>,
        ) {
            let _service = ReadReceiptService::new(settings_repo, receipt_aggregator);
        }
    }

    #[test]
    fn test_get_last_read_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown channel/user should return None
        let user_id: UserId = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let result = service.get_last_read("unknown-channel", &user_id);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_channel_read_states_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown channel should return empty map
        let states = service.get_channel_read_states("unknown-channel");
        assert!(states.is_empty());
    }

    #[test]
    fn test_process_read_receipt() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let user_id: UserId = user_id_str.parse().unwrap();

        // Process a read receipt
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-100", 1000));

        // Should now have a read state
        let last_read = service.get_last_read("channel-1", &user_id);
        assert_eq!(last_read, Some("msg-100".to_string()));
    }

    #[test]
    fn test_process_read_receipt_updates_high_water_mark() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let user_id: UserId = user_id_str.parse().unwrap();

        // Process older receipt
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-100", 1000));

        // Process newer receipt
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-200", 2000));

        // Should have newer message
        let last_read = service.get_last_read("channel-1", &user_id);
        assert_eq!(last_read, Some("msg-200".to_string()));
    }

    #[test]
    fn test_older_receipt_ignored() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let user_id: UserId = user_id_str.parse().unwrap();

        // Process newer receipt first
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-200", 2000));

        // Process older receipt
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-100", 1000));

        // Should still have newer message
        let last_read = service.get_last_read("channel-1", &user_id);
        assert_eq!(last_read, Some("msg-200".to_string()));
    }

    #[test]
    fn test_get_channel_read_states() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user1_str = "550e8400-e29b-41d4-a716-446655440001";
        let user2_str = "550e8400-e29b-41d4-a716-446655440002";

        // Add receipts for multiple users
        service.process_read_receipt(make_receipt("channel-1", user1_str, "msg-100", 1000));
        service.process_read_receipt(make_receipt("channel-1", user2_str, "msg-50", 900));

        let states = service.get_channel_read_states("channel-1");
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn test_get_readers_of_message() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user1_str = "550e8400-e29b-41d4-a716-446655440001";
        let user2_str = "550e8400-e29b-41d4-a716-446655440002";
        let user3_str = "550e8400-e29b-41d4-a716-446655440003";

        // Add receipts - 2 users at msg-100, 1 at msg-50
        service.process_read_receipt(make_receipt("channel-1", user1_str, "msg-100", 1000));
        service.process_read_receipt(make_receipt("channel-1", user2_str, "msg-100", 1100));
        service.process_read_receipt(make_receipt("channel-1", user3_str, "msg-50", 900));

        let readers = service.get_readers_of_message("channel-1", "msg-100");
        assert_eq!(readers.len(), 2);
    }

    #[test]
    fn test_clear_channel() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let user_id: UserId = user_id_str.parse().unwrap();

        // Add receipts to two channels
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-100", 1000));
        service.process_read_receipt(make_receipt("channel-2", user_id_str, "msg-200", 2000));

        // Clear only channel-1
        service.clear_channel("channel-1");

        // channel-1 should be empty
        assert!(service.get_last_read("channel-1", &user_id).is_none());
        // channel-2 should still have data
        assert!(service.get_last_read("channel-2", &user_id).is_some());
    }

    #[test]
    fn test_clear_all() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id_str = "550e8400-e29b-41d4-a716-446655440000";
        let user_id: UserId = user_id_str.parse().unwrap();

        // Add receipts
        service.process_read_receipt(make_receipt("channel-1", user_id_str, "msg-100", 1000));
        service.process_read_receipt(make_receipt("channel-2", user_id_str, "msg-200", 2000));

        // Clear all
        service.clear_all();

        // Both should be empty
        assert!(service.get_last_read("channel-1", &user_id).is_none());
        assert!(service.get_last_read("channel-2", &user_id).is_none());
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
        service.process_read_receipt(make_receipt(
            "channel-1",
            "550e8400-e29b-41d4-a716-446655440000",
            "msg-100",
            1000,
        ));

        // Should receive the receipt
        let received = rx.try_recv().unwrap();
        assert_eq!(received.channel_id, "channel-1");
        assert_eq!(received.last_read_id, "msg-100");
    }
}
