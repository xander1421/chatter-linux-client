//! Presence service.
//!
//! Online/offline/away/DND presence management.
//!
//! Orchestrates presence updates to the server and provides
//! local cache queries for presence lookups without network calls.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::broadcast;

use crate::domain::ids::UserId;
use crate::domain::user::PresenceStatus;
use crate::domain::anon_dm::AnonDmId;
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::user_client::UserClient;
use crate::network::streaming::presence_stream::PresenceRouter;
use crate::proto::chat_v1::{PresenceUpdate, UserPresence};
use crate::proto::UserPresence as ProtoUserPresence;
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::services::anon_dm::AnonDmService;
use crate::storage::repositories::settings_repo::SettingsRepository;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;

/// Presence service errors.
#[derive(Debug, thiserror::Error)]
pub enum PresenceError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Invalid presence status.
    #[error("invalid status")]
    InvalidStatus,
}

impl From<CurrentUserError> for PresenceError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => PresenceError::Storage(e),
            CurrentUserError::NotLoggedIn => PresenceError::NotLoggedIn,
        }
    }
}

/// Presence service for managing user presence.
///
/// Orchestrates presence updates and provides local cache queries.
/// Uses the PresenceRouter for caching and broadcasting updates.
///
/// # Example
///
/// ```ignore
/// let service = PresenceService::new(settings_repo, presence_router);
///
/// // Update presence to online
/// service.update_presence(&mut user_client, PresenceStatus::Online).await?;
///
/// // Check if a user is online (local cache, no network)
/// if service.is_online(&friend_id) {
///     println!("Friend is online!");
/// }
/// ```
#[derive(Clone)]
pub struct PresenceService {
    settings_repo: Arc<SettingsRepository>,
    presence_router: Arc<PresenceRouter>,
    /// Anonymous DM service for presence updates (optional)
    anon_dm_service: Option<Arc<AnonDmService>>,
    /// Current user presence for anon DMs
    anon_dm_presence: Arc<RwLock<ProtoUserPresence>>,
    /// Last activity timestamp for idle detection
    last_activity: Arc<RwLock<Instant>>,
}

impl PresenceService {
    /// Create a new PresenceService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        presence_router: Arc<PresenceRouter>,
    ) -> Self {
        Self {
            settings_repo,
            presence_router,
            anon_dm_service: None,
            anon_dm_presence: Arc::new(RwLock::new(ProtoUserPresence::Offline)),
            last_activity: Arc::new(RwLock::new(Instant::now())),
        }
    }
    
    /// Set the anonymous DM service for presence updates.
    pub fn with_anon_dm_service(mut self, service: Arc<AnonDmService>) -> Self {
        self.anon_dm_service = Some(service);
        self
    }

    // ========================================================================
    // Server Operations
    // ========================================================================

    /// Update current user's presence status.
    ///
    /// Calls the server to update presence, then updates the local cache
    /// on success.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to user client
    /// * `status` - New presence status
    pub async fn update_presence(
        &self,
        client: &mut UserClient,
        status: PresenceStatus,
    ) -> Result<(), PresenceError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Convert domain status to proto
        let proto_presence = domain_presence_to_proto(status);

        // Call server (unary, returns empty response)
        client.update_presence(proto_presence).await?;

        // Update local cache with new status
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let update = PresenceUpdate {
            user_id: current_user.to_string(),
            presence: proto_presence as i32,
            timestamp: now_ms,
        };
        self.presence_router.route(update);

        Ok(())
    }

    /// Start the presence stream for receiving friend presence updates.
    ///
    /// Consumes a boxed stream of presence updates and spawns a task to route
    /// them through the presence router. The caller should obtain the stream
    /// from the UserClient and box it before calling this method.
    ///
    /// Returns a JoinHandle for the spawned stream task, allowing the caller
    /// to track or cancel the stream if needed.
    ///
    /// # Arguments
    ///
    /// * `stream` - Boxed stream of presence updates (from `UserClient::stream_presence()`)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stream = user_client.stream_presence().await?;
    /// let handle = presence_service.start_presence_stream(Box::pin(stream))?;
    /// ```
    pub fn start_presence_stream(
        &self,
        stream: std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<PresenceUpdate, NetworkError>> + Send>,
        >,
    ) -> Result<tokio::task::JoinHandle<()>, PresenceError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        let mut boxed_stream = stream;

        // Clone router for the spawned task
        let router = Arc::clone(&self.presence_router);

        // Spawn task to consume stream
        let handle = tokio::spawn(async move {
            while let Some(result) = boxed_stream.next().await {
                match result {
                    Ok(update) => {
                        router.route(update);
                    }
                    Err(e) => {
                        // Log error and continue (stream may recover)
                        tracing::warn!("Presence stream error: {}", e);
                    }
                }
            }
            tracing::info!("Presence stream ended");
        });

        Ok(handle)
    }

    // ========================================================================
    // Subscription
    // ========================================================================

    /// Subscribe to presence update events.
    ///
    /// Returns a broadcast receiver for presence updates. Use this to
    /// react to presence changes in the UI layer.
    pub fn subscribe(&self) -> broadcast::Receiver<PresenceUpdate> {
        self.presence_router.on_presence_update()
    }

    // ========================================================================
    // Local Query Methods (no network, no auth check needed)
    // ========================================================================

    /// Get presence status for a user from local cache.
    ///
    /// Returns None if user not in cache.
    ///
    /// # Arguments
    ///
    /// * `user_id` - ID of user to check
    pub fn get_presence(&self, user_id: &UserId) -> Option<PresenceStatus> {
        self.presence_router
            .get_presence(&user_id.to_string())
            .map(|cached| proto_presence_to_domain(cached.status))
    }

    /// Get presence status for multiple users from local cache.
    ///
    /// Returns a map of user_id -> presence for users found in cache.
    /// Users not in cache will not be included in the result.
    ///
    /// # Arguments
    ///
    /// * `user_ids` - IDs of users to check
    pub fn get_many_presence(&self, user_ids: &[&UserId]) -> HashMap<UserId, PresenceStatus> {
        // Convert UserId refs to string refs for router
        let id_strings: Vec<String> = user_ids.iter().map(|id| id.to_string()).collect();
        let id_strs: Vec<&str> = id_strings.iter().map(|s| s.as_str()).collect();

        // Get from router (returns HashMap<String, CachedPresence>)
        let cached = self.presence_router.get_many_presence(&id_strs);

        // Convert back to domain types
        cached
            .into_iter()
            .filter_map(|(id_str, cached_presence)| {
                id_str.parse::<UserId>().ok().map(|user_id| {
                    (user_id, proto_presence_to_domain(cached_presence.status))
                })
            })
            .collect()
    }

    /// Check if a user is currently online.
    ///
    /// Returns false if user not in cache or not online.
    ///
    /// # Arguments
    ///
    /// * `user_id` - ID of user to check
    pub fn is_online(&self, user_id: &UserId) -> bool {
        self.presence_router.is_online(&user_id.to_string())
    }

    // ========================================================================
    // Cache Management
    // ========================================================================

    /// Seed the presence cache with initial data.
    ///
    /// Call after connection with bulk presence fetch.
    ///
    /// # Arguments
    ///
    /// * `entries` - List of (user_id, status, timestamp) tuples
    pub fn seed_presence(&self, entries: Vec<(UserId, PresenceStatus, u64)>) {
        // Convert domain types to proto types for router
        let proto_entries: Vec<(String, UserPresence, u64)> = entries
            .into_iter()
            .map(|(user_id, status, timestamp)| {
                (
                    user_id.to_string(),
                    domain_presence_to_proto(status),
                    timestamp,
                )
            })
            .collect();

        self.presence_router.seed_cache(proto_entries);
    }

    /// Clear the presence cache.
    ///
    /// Call when disconnecting or logging out.
    pub fn clear_presence(&self) {
        self.presence_router.clear_cache();
    }
    
    // ========================================================================
    // Anonymous DM Presence Management
    // ========================================================================
    
    /// Initialize presence for anonymous DMs (send online status).
    pub async fn initialize_anon_dm_presence(&self) {
        self.set_anon_dm_presence(ProtoUserPresence::Online).await;
    }
    
    /// Shutdown presence for anonymous DMs (send offline status).
    pub async fn shutdown_anon_dm_presence(&self) {
        self.set_anon_dm_presence(ProtoUserPresence::Offline).await;
    }
    
    /// Set presence for anonymous DMs and broadcast to all conversations.
    pub async fn set_anon_dm_presence(&self, presence: ProtoUserPresence) {
        // Update local state
        {
            let mut current = self.anon_dm_presence.write().await;
            if *current == presence {
                return; // No change
            }
            *current = presence;
        }
        
        tracing::info!("Setting anon DM presence to {:?}", presence);
        
        // Broadcast to all conversations
        self.broadcast_anon_dm_presence(presence).await;
        
        // Reset activity tracking if going online
        if presence == ProtoUserPresence::Online {
            *self.last_activity.write().await = Instant::now();
        }
    }
    
    /// Get current anonymous DM presence.
    pub async fn get_anon_dm_presence(&self) -> ProtoUserPresence {
        *self.anon_dm_presence.read().await
    }
    
    /// Track user activity for idle detection.
    pub async fn track_activity(&self) {
        let mut last = self.last_activity.write().await;
        *last = Instant::now();
        
        // If user was away, bring them back online
        let current = *self.anon_dm_presence.read().await;
        if current == ProtoUserPresence::Away {
            drop(last); // Release lock before calling set_presence
            tracing::debug!("User activity detected, going online");
            self.set_anon_dm_presence(ProtoUserPresence::Online).await;
        }
    }
    
    /// Start idle detection background task.
    /// 
    /// Returns a JoinHandle for the spawned task.
    pub fn start_idle_detection(&self, idle_timeout: Duration) -> tokio::task::JoinHandle<()> {
        let anon_dm_presence = self.anon_dm_presence.clone();
        let last_activity = self.last_activity.clone();
        let service = self.clone();
        
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(30));
            
            loop {
                ticker.tick().await;
                
                let last = *last_activity.read().await;
                let current = *anon_dm_presence.read().await;
                
                if current == ProtoUserPresence::Online && last.elapsed() > idle_timeout {
                    tracing::debug!("User idle for {:?}, setting away status", idle_timeout);
                    service.set_anon_dm_presence(ProtoUserPresence::Away).await;
                }
            }
        })
    }
    
    /// Send presence update when opening a specific anon DM chat.
    pub async fn notify_anon_dm_opened(&self, conversation_id: &AnonDmId) {
        let current = self.get_anon_dm_presence().await;
        if current == ProtoUserPresence::Offline {
            return;
        }
        
        if let Some(service) = &self.anon_dm_service {
            if let Err(e) = service.send_presence_update(conversation_id, current).await {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    error = %e,
                    "Failed to send presence update for anon DM open"
                );
            }
        }
    }
    
    /// Broadcast presence to all anonymous DM conversations.
    async fn broadcast_anon_dm_presence(&self, presence: ProtoUserPresence) {
        let Some(service) = &self.anon_dm_service else {
            tracing::debug!("No anon DM service configured, skipping presence broadcast");
            return;
        };
        
        let conversations = match service.list_conversations() {
            Ok(convs) => convs,
            Err(e) => {
                tracing::warn!("Failed to list anon DM conversations: {}", e);
                return;
            }
        };
        
        let ready_count = conversations.iter().filter(|c| c.is_ready()).count();
        tracing::info!(
            "Broadcasting anon DM presence {:?} to {} ready conversations",
            presence,
            ready_count
        );
        
        for conv in &conversations {
            if !conv.is_ready() {
                continue;
            }
            
            let conv_id = conv.id;
            let service = service.clone();
            
            tokio::spawn(async move {
                if let Err(e) = service.send_presence_update(&conv_id, presence).await {
                    tracing::debug!(
                        conversation_id = %conv_id,
                        error = %e,
                        "Failed to send anon DM presence update"
                    );
                }
            });
        }
    }
}

// ============================================================================
// Proto <-> Domain Conversion Helpers
// ============================================================================

/// Convert domain PresenceStatus to proto UserPresence.
fn domain_presence_to_proto(status: PresenceStatus) -> UserPresence {
    match status {
        PresenceStatus::Offline => UserPresence::Offline,
        PresenceStatus::Online => UserPresence::Online,
        PresenceStatus::Away => UserPresence::Away,
        PresenceStatus::DoNotDisturb => UserPresence::DoNotDisturb,
    }
}

/// Convert proto UserPresence to domain PresenceStatus.
fn proto_presence_to_domain(proto: UserPresence) -> PresenceStatus {
    match proto {
        UserPresence::PresenceUnspecified => PresenceStatus::Offline,
        UserPresence::Offline => PresenceStatus::Offline,
        UserPresence::Online => PresenceStatus::Online,
        UserPresence::Away => PresenceStatus::Away,
        UserPresence::DoNotDisturb => PresenceStatus::DoNotDisturb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::tempdir;

    /// Create a test PresenceService with a proper temporary database.
    ///
    /// Uses tempfile to create a unique temporary directory that is
    /// automatically cleaned up when the test completes. The database
    /// runs migrations to set up the schema properly.
    fn create_test_service() -> (PresenceService, Arc<PresenceRouter>, tempfile::TempDir) {
        let router = Arc::new(PresenceRouter::new());

        // Create a unique temp directory - will be cleaned up when dropped
        let temp_dir = tempdir().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");

        let db = Database::open(&db_path, "test-passphrase")
            .expect("Failed to open test database");

        // Only run the minimal migrations needed (skip v008 since v001 creates settings)
        // We just run the V001Initial which creates the settings table
        {
            let conn = db.connection().expect("Failed to get connection");
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS schema_version (
                    version INTEGER PRIMARY KEY,
                    description TEXT NOT NULL,
                    applied_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS sync_state (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                "#,
            )
            .expect("Failed to create tables");
        }

        let settings_repo = Arc::new(SettingsRepository::new(Arc::new(db)));

        let service = PresenceService::new(settings_repo, Arc::clone(&router));
        // Return temp_dir to keep it alive for the duration of the test
        (service, router, temp_dir)
    }

    #[test]
    fn test_presence_error_display() {
        let err = PresenceError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = PresenceError::InvalidStatus;
        assert!(err.to_string().contains("invalid status"));
    }

    #[test]
    fn test_domain_presence_to_proto() {
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::Offline),
            UserPresence::Offline
        );
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::Online),
            UserPresence::Online
        );
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::Away),
            UserPresence::Away
        );
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::DoNotDisturb),
            UserPresence::DoNotDisturb
        );
    }

    #[test]
    fn test_proto_presence_to_domain() {
        assert_eq!(
            proto_presence_to_domain(UserPresence::Offline),
            PresenceStatus::Offline
        );
        assert_eq!(
            proto_presence_to_domain(UserPresence::Online),
            PresenceStatus::Online
        );
        assert_eq!(
            proto_presence_to_domain(UserPresence::Away),
            PresenceStatus::Away
        );
        assert_eq!(
            proto_presence_to_domain(UserPresence::DoNotDisturb),
            PresenceStatus::DoNotDisturb
        );
        // Unspecified defaults to Offline
        assert_eq!(
            proto_presence_to_domain(UserPresence::PresenceUnspecified),
            PresenceStatus::Offline
        );
    }

    #[test]
    fn test_presence_roundtrip() {
        for status in [
            PresenceStatus::Offline,
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::DoNotDisturb,
        ] {
            let proto = domain_presence_to_proto(status);
            let back = proto_presence_to_domain(proto);
            assert_eq!(back, status);
        }
    }

    #[test]
    fn test_presence_service_construction() {
        // Verify the PresenceService struct type signatures compile correctly.
        fn _verify_types(
            settings_repo: Arc<SettingsRepository>,
            presence_router: Arc<PresenceRouter>,
        ) {
            let _service = PresenceService::new(settings_repo, presence_router);
        }
    }

    #[test]
    fn test_get_presence_unknown_user() {
        let (service, _router, _temp_dir) = create_test_service();

        // Unknown user should return None
        let unknown_id = UserId::new();
        assert!(service.get_presence(&unknown_id).is_none());
    }

    #[test]
    fn test_is_online_unknown_user() {
        let (service, _router, _temp_dir) = create_test_service();

        // Unknown user should return false
        let unknown_id = UserId::new();
        assert!(!service.is_online(&unknown_id));
    }

    #[test]
    fn test_subscribe_returns_receiver() {
        let (service, router, _temp_dir) = create_test_service();

        // Should be able to subscribe
        let _rx = service.subscribe();

        // Subscriber count should increase
        assert_eq!(router.subscriber_count(), 1);
    }

    #[test]
    fn test_seed_presence_populates_cache() {
        let (service, router, _temp_dir) = create_test_service();

        // Seed some presence data
        let user1 = UserId::new();
        let user2 = UserId::new();

        service.seed_presence(vec![
            (user1.clone(), PresenceStatus::Online, 1000),
            (user2.clone(), PresenceStatus::Away, 2000),
        ]);

        // Verify cache was populated
        assert_eq!(router.cache_size(), 2);
        assert_eq!(service.get_presence(&user1), Some(PresenceStatus::Online));
        assert_eq!(service.get_presence(&user2), Some(PresenceStatus::Away));
    }

    #[test]
    fn test_clear_presence_empties_cache() {
        let (service, router, _temp_dir) = create_test_service();

        // Seed some presence data
        let user1 = UserId::new();
        service.seed_presence(vec![(user1.clone(), PresenceStatus::Online, 1000)]);

        assert_eq!(router.cache_size(), 1);

        // Clear presence
        service.clear_presence();

        // Cache should be empty
        assert_eq!(router.cache_size(), 0);
        assert!(service.get_presence(&user1).is_none());
    }

    #[test]
    fn test_get_many_presence() {
        let (service, _router, _temp_dir) = create_test_service();

        // Seed some presence data
        let user1 = UserId::new();
        let user2 = UserId::new();
        let user3 = UserId::new(); // Not seeded

        service.seed_presence(vec![
            (user1.clone(), PresenceStatus::Online, 1000),
            (user2.clone(), PresenceStatus::DoNotDisturb, 2000),
        ]);

        // Query multiple users
        let result = service.get_many_presence(&[&user1, &user2, &user3]);

        // Should have 2 results (user3 not in cache)
        assert_eq!(result.len(), 2);
        assert_eq!(result.get(&user1), Some(&PresenceStatus::Online));
        assert_eq!(result.get(&user2), Some(&PresenceStatus::DoNotDisturb));
        assert!(result.get(&user3).is_none());
    }

    #[test]
    fn test_is_online_with_seeded_data() {
        let (service, _router, _temp_dir) = create_test_service();

        let online_user = UserId::new();
        let offline_user = UserId::new();
        let away_user = UserId::new();

        service.seed_presence(vec![
            (online_user.clone(), PresenceStatus::Online, 1000),
            (offline_user.clone(), PresenceStatus::Offline, 2000),
            (away_user.clone(), PresenceStatus::Away, 3000),
        ]);

        // Only online users should return true
        assert!(service.is_online(&online_user));
        assert!(!service.is_online(&offline_user));
        assert!(!service.is_online(&away_user));
    }
}
