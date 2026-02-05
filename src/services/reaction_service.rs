//! Reaction service.
//!
//! Orchestrates reaction operations using existing network clients,
//! streaming aggregators, and local storage. Provides server operations,
//! local queries, and subscription APIs for real-time reaction updates.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::ids::{MessageId, UserId};
use crate::domain::message::ReactionCount;
use crate::domain::reaction::{Emoji, Reaction};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::message_client::MessageClient;
use crate::network::streaming::reaction_stream::{ReactionAggregator, ReactionEvent};
use crate::proto::chat_v1::{Message, ReactionUpdate};
use crate::storage::repositories::reaction_repo::ReactionRepository;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Reaction service errors.
#[derive(Debug, thiserror::Error)]
pub enum ReactionError {
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

/// Reaction service.
///
/// Orchestrates reaction operations using network clients, streaming aggregators,
/// and local storage. Provides server-first operations and local cache queries.
///
/// # Example
///
/// ```ignore
/// let service = ReactionService::new(settings_repo, reaction_repo, reaction_aggregator);
///
/// // Add a reaction via server
/// let updated_msg = service.add_reaction(&mut message_client, "msg-123", "channel-456", "👍").await?;
///
/// // Query reactions locally (no network)
/// let reactions = service.get_reactions_local(&message_id)?;
///
/// // Subscribe to real-time reaction events
/// let mut rx = service.subscribe_added();
/// ```
pub struct ReactionService {
    settings_repo: Arc<SettingsRepository>,
    reaction_repo: Arc<ReactionRepository>,
    reaction_aggregator: Arc<ReactionAggregator>,
}

impl ReactionService {
    /// Create a new ReactionService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        reaction_repo: Arc<ReactionRepository>,
        reaction_aggregator: Arc<ReactionAggregator>,
    ) -> Self {
        Self {
            settings_repo,
            reaction_repo,
            reaction_aggregator,
        }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<UserId, ReactionError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(ReactionError::Storage)?
            .ok_or(ReactionError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| ReactionError::NotLoggedIn)
    }

    // ========================================================================
    // Server Operations
    // ========================================================================

    /// Add a reaction to a message.
    ///
    /// Sends the reaction to the server and returns the updated message.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `message_id` - ID of the message to react to
    /// * `channel_id` - Channel containing the message
    /// * `emoji` - Emoji reaction (Unicode string, e.g., "👍")
    pub async fn add_reaction(
        &self,
        client: &mut MessageClient,
        message_id: &str,
        channel_id: &str,
        emoji: &str,
    ) -> Result<Message, ReactionError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Send reaction to server
        let message = client.add_reaction(message_id, channel_id, emoji).await?;

        Ok(message)
    }

    /// Remove a reaction from a message.
    ///
    /// Removes the reaction on the server and returns the updated message.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `message_id` - ID of the message to remove reaction from
    /// * `channel_id` - Channel containing the message
    /// * `emoji` - Emoji reaction to remove
    pub async fn remove_reaction(
        &self,
        client: &mut MessageClient,
        message_id: &str,
        channel_id: &str,
        emoji: &str,
    ) -> Result<Message, ReactionError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Remove reaction on server
        let message = client.remove_reaction(message_id, channel_id, emoji).await?;

        Ok(message)
    }

    /// Get reactions for a message from the server.
    ///
    /// Fetches reactions from the server with pagination support.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `message_id` - ID of the message to get reactions for
    /// * `channel_id` - Channel containing the message
    /// * `limit` - Maximum number of reactions to return
    /// * `offset` - Pagination offset
    /// * `emoji_filter` - Optional filter by specific emoji
    pub async fn get_reactions(
        &self,
        client: &mut MessageClient,
        message_id: &str,
        channel_id: &str,
        limit: i32,
        offset: i32,
        emoji_filter: Option<&str>,
    ) -> Result<Vec<Reaction>, ReactionError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Fetch from server
        let response = client
            .get_message_reactions(message_id, channel_id, limit, offset, emoji_filter)
            .await?;

        // Convert proto reactions to domain reactions
        let message_id: MessageId = message_id
            .parse()
            .map_err(|_| ReactionError::Storage(StorageError::InvalidData(
                "Invalid message ID".to_string(),
            )))?;

        let reactions: Vec<Reaction> = response
            .reactions
            .iter()
            .filter_map(|proto| proto_to_domain_reaction(proto, &message_id).ok())
            .collect();

        Ok(reactions)
    }

    // ========================================================================
    // Stream Handling
    // ========================================================================

    /// Process an incoming reaction event from the stream.
    ///
    /// Delegates to the reaction aggregator to update local state and
    /// broadcast changes to subscribers.
    ///
    /// # Arguments
    ///
    /// * `update` - The reaction update event from the stream
    /// * `user_id` - User who made the reaction (from envelope context)
    pub fn process_reaction_event(&self, update: ReactionUpdate, user_id: String) {
        self.reaction_aggregator.process(update, user_id);
    }

    // ========================================================================
    // Subscription
    // ========================================================================

    /// Subscribe to reaction added events.
    ///
    /// Returns a broadcast receiver for reaction added events.
    pub fn subscribe_added(&self) -> broadcast::Receiver<ReactionEvent> {
        self.reaction_aggregator.on_reaction_added()
    }

    /// Subscribe to reaction removed events.
    ///
    /// Returns a broadcast receiver for reaction removed events.
    pub fn subscribe_removed(&self) -> broadcast::Receiver<ReactionEvent> {
        self.reaction_aggregator.on_reaction_removed()
    }

    // ========================================================================
    // Local Query Methods (no network, no auth check needed)
    // ========================================================================

    /// Get reactions for a message from local storage.
    ///
    /// Returns reactions from the local database without network access.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to get reactions for
    pub fn get_reactions_local(&self, message_id: &MessageId) -> Result<Vec<Reaction>, ReactionError> {
        self.reaction_repo
            .list_for_message(message_id)
            .map_err(ReactionError::Storage)
    }

    /// Get reaction counts for a message from local storage.
    ///
    /// Returns aggregated reaction counts per emoji from local database.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to get counts for
    pub fn get_reaction_counts_local(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<ReactionCount>, ReactionError> {
        self.reaction_repo
            .get_reaction_counts(message_id)
            .map_err(ReactionError::Storage)
    }

    /// Toggle a reaction in local storage.
    ///
    /// Adds the reaction if it doesn't exist, removes it if it does.
    /// Returns `true` if the reaction was added, `false` if removed.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to toggle reaction on
    /// * `emoji` - Emoji to toggle
    pub fn toggle_reaction_local(
        &self,
        message_id: &MessageId,
        emoji: &Emoji,
    ) -> Result<bool, ReactionError> {
        // Get current user for the reaction
        let user_id = self.current_user_id()?;

        self.reaction_repo
            .toggle(message_id, &user_id, emoji)
            .map_err(ReactionError::Storage)
    }

    // ========================================================================
    // Cache Management
    // ========================================================================

    /// Clear the reaction aggregator cache.
    ///
    /// Call when disconnecting or logging out.
    pub fn clear(&self) {
        self.reaction_aggregator.clear();
    }

    /// Get recent reactions for a message from the aggregator cache.
    ///
    /// Returns cached reaction events, useful for recent activity display.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to get recent reactions for
    pub fn get_recent_reactions(&self, message_id: &str) -> Vec<ReactionEvent> {
        self.reaction_aggregator.get_recent_reactions(message_id)
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Convert a proto Reaction to a domain Reaction.
///
/// Creates a domain Reaction from the proto representation.
/// Uses the proto's user_id, emoji, and timestamp fields.
fn proto_to_domain_reaction(
    proto: &crate::proto::chat_v1::Reaction,
    message_id: &MessageId,
) -> Result<Reaction, ReactionError> {
    // Parse user_id
    let user_id: UserId = proto
        .user_id
        .parse()
        .map_err(|_| ReactionError::Storage(StorageError::InvalidData(
            format!("Invalid user ID: {}", proto.user_id),
        )))?;

    // Create emoji (assume unicode if no custom indicator)
    let emoji = Emoji::unicode(&proto.emoji);

    // Convert timestamp (proto uses u64 millis)
    let created_at = chrono::DateTime::from_timestamp_millis(proto.timestamp as i64)
        .unwrap_or_else(chrono::Utc::now);

    Ok(Reaction {
        id: crate::domain::ids::ReactionId::new(),
        message_id: *message_id,
        user_id,
        emoji,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::tempdir;

    /// Create a test ReactionService with a proper temporary database.
    fn create_test_service() -> (ReactionService, Arc<ReactionAggregator>, tempfile::TempDir) {
        let aggregator = Arc::new(ReactionAggregator::new());

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
                CREATE TABLE IF NOT EXISTS reactions (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL,
                    user_id TEXT NOT NULL,
                    emoji_json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                "#,
            )
            .expect("Failed to create tables");
        }

        let db_arc = Arc::new(db);
        let settings_repo = Arc::new(SettingsRepository::new(Arc::clone(&db_arc)));
        let reaction_repo = Arc::new(ReactionRepository::new(Arc::clone(&db_arc)));

        let service = ReactionService::new(settings_repo, reaction_repo, Arc::clone(&aggregator));
        (service, aggregator, temp_dir)
    }

    /// Helper to set up a logged-in user.
    fn set_logged_in_user(service: &ReactionService, user_id: &str) {
        service
            .settings_repo
            .set_string(keys::CURRENT_USER_ID, user_id)
            .expect("Failed to set user ID");
    }

    #[test]
    fn test_reaction_error_display() {
        let err = ReactionError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let network_err = ReactionError::Network(NetworkError::ConnectionFailed(
            "test".to_string(),
        ));
        assert!(network_err.to_string().contains("network error"));
    }

    #[test]
    fn test_reaction_service_construction() {
        // Verify the ReactionService struct type signatures compile correctly.
        fn _verify_types(
            settings_repo: Arc<SettingsRepository>,
            reaction_repo: Arc<ReactionRepository>,
            reaction_aggregator: Arc<ReactionAggregator>,
        ) {
            let _service = ReactionService::new(settings_repo, reaction_repo, reaction_aggregator);
        }
    }

    #[test]
    fn test_get_reactions_local_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown message should return empty vec
        let message_id = MessageId::new();
        let reactions = service.get_reactions_local(&message_id).unwrap();
        assert!(reactions.is_empty());
    }

    #[test]
    fn test_get_reaction_counts_local_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown message should return empty vec
        let message_id = MessageId::new();
        let counts = service.get_reaction_counts_local(&message_id).unwrap();
        assert!(counts.is_empty());
    }

    #[test]
    fn test_toggle_reaction_local_requires_login() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let message_id = MessageId::new();
        let emoji = Emoji::unicode("👍");

        // Should fail without login
        let result = service.toggle_reaction_local(&message_id, &emoji);
        assert!(matches!(result, Err(ReactionError::NotLoggedIn)));
    }

    #[test]
    fn test_toggle_reaction_local_adds_reaction() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id = "550e8400-e29b-41d4-a716-446655440000";
        set_logged_in_user(&service, user_id);

        let message_id = MessageId::new();
        let emoji = Emoji::unicode("👍");

        // First toggle should add (return true)
        let added = service.toggle_reaction_local(&message_id, &emoji).unwrap();
        assert!(added);

        // Should now have a reaction
        let reactions = service.get_reactions_local(&message_id).unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, emoji);
    }

    #[test]
    fn test_toggle_reaction_local_removes_reaction() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id = "550e8400-e29b-41d4-a716-446655440000";
        set_logged_in_user(&service, user_id);

        let message_id = MessageId::new();
        let emoji = Emoji::unicode("👍");

        // First toggle adds
        let added = service.toggle_reaction_local(&message_id, &emoji).unwrap();
        assert!(added);

        // Second toggle removes (return false)
        let removed = service.toggle_reaction_local(&message_id, &emoji).unwrap();
        assert!(!removed);

        // Should now have no reactions
        let reactions = service.get_reactions_local(&message_id).unwrap();
        assert!(reactions.is_empty());
    }

    #[test]
    fn test_subscribe_added_returns_receiver() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Should be able to subscribe
        let _rx = service.subscribe_added();
    }

    #[test]
    fn test_subscribe_removed_returns_receiver() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Should be able to subscribe
        let _rx = service.subscribe_removed();
    }

    #[test]
    fn test_process_reaction_event_delegates_to_aggregator() {
        let (service, aggregator, _temp_dir) = create_test_service();
        let _rx = aggregator.on_reaction_added();

        // Process an event
        service.process_reaction_event(
            ReactionUpdate {
                message_id: "msg-1".to_string(),
                emoji: "👍".to_string(),
                add: true,
                proof_of_work: vec![],
            },
            "user-1".to_string(),
        );

        // Should have cached the reaction
        let recent = service.get_recent_reactions("msg-1");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].update.emoji, "👍");
        assert_eq!(recent[0].user_id, "user-1");
    }

    #[test]
    fn test_clear_clears_aggregator_cache() {
        let (service, aggregator, _temp_dir) = create_test_service();
        let _rx = aggregator.on_reaction_added();

        // Add some reactions
        service.process_reaction_event(
            ReactionUpdate {
                message_id: "msg-1".to_string(),
                emoji: "👍".to_string(),
                add: true,
                proof_of_work: vec![],
            },
            "user-1".to_string(),
        );

        assert_eq!(aggregator.cache_size(), 1);

        // Clear
        service.clear();

        assert_eq!(aggregator.cache_size(), 0);
    }

    #[test]
    fn test_get_recent_reactions_empty() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        // Unknown message should return empty vec
        let recent = service.get_recent_reactions("unknown-msg");
        assert!(recent.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_receives_added_events() {
        let (service, _aggregator, _temp_dir) = create_test_service();
        let mut rx = service.subscribe_added();

        // Process an add event
        service.process_reaction_event(
            ReactionUpdate {
                message_id: "msg-1".to_string(),
                emoji: "❤️".to_string(),
                add: true,
                proof_of_work: vec![],
            },
            "user-1".to_string(),
        );

        // Should receive the event
        let received = rx.try_recv().unwrap();
        assert_eq!(received.update.message_id, "msg-1");
        assert_eq!(received.update.emoji, "❤️");
        assert_eq!(received.user_id, "user-1");
    }

    #[tokio::test]
    async fn test_subscribe_receives_removed_events() {
        let (service, _aggregator, _temp_dir) = create_test_service();
        let mut rx = service.subscribe_removed();

        // Process a remove event
        service.process_reaction_event(
            ReactionUpdate {
                message_id: "msg-1".to_string(),
                emoji: "👎".to_string(),
                add: false,
                proof_of_work: vec![],
            },
            "user-1".to_string(),
        );

        // Should receive the event
        let received = rx.try_recv().unwrap();
        assert_eq!(received.update.message_id, "msg-1");
        assert_eq!(received.update.emoji, "👎");
        assert!(!received.update.add);
    }

    #[test]
    fn test_proto_to_domain_reaction() {
        let proto = crate::proto::chat_v1::Reaction {
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            emoji: "🎉".to_string(),
            timestamp: 1234567890000,
        };

        let message_id = MessageId::new();
        let reaction = proto_to_domain_reaction(&proto, &message_id).unwrap();

        assert_eq!(reaction.message_id, message_id);
        assert_eq!(reaction.emoji.value, "🎉");
        assert!(!reaction.emoji.is_custom);
    }

    #[test]
    fn test_proto_to_domain_reaction_invalid_user_id() {
        let proto = crate::proto::chat_v1::Reaction {
            user_id: "not-a-uuid".to_string(),
            emoji: "👍".to_string(),
            timestamp: 1234567890000,
        };

        let message_id = MessageId::new();
        let result = proto_to_domain_reaction(&proto, &message_id);

        assert!(matches!(result, Err(ReactionError::Storage(_))));
    }

    #[test]
    fn test_reaction_counts_aggregation() {
        let (service, _aggregator, _temp_dir) = create_test_service();

        let user_id = "550e8400-e29b-41d4-a716-446655440000";
        set_logged_in_user(&service, user_id);

        let message_id = MessageId::new();

        // Add reactions
        let thumbs_up = Emoji::unicode("👍");
        let heart = Emoji::unicode("❤️");

        // Add via repository directly for testing
        service
            .reaction_repo
            .insert(&Reaction::new(message_id, user_id.parse().unwrap(), thumbs_up.clone()))
            .unwrap();
        service
            .reaction_repo
            .insert(&Reaction::new(message_id, user_id.parse().unwrap(), heart.clone()))
            .unwrap();

        // Get counts
        let counts = service.get_reaction_counts_local(&message_id).unwrap();
        assert_eq!(counts.len(), 2);

        // Each should have count 1
        for count in &counts {
            assert_eq!(count.count, 1);
        }
    }
}
