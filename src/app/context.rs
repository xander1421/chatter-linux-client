//! Application context.
//!
//! Central dependency container that initializes all application subsystems
//! in the correct order and provides access to them.

use std::env;
use std::sync::Arc;

use crate::crypto::signal::{IdentityKeyManager, PreKeyManager, SenderKeyManager, SessionManager};
use crate::error::{NetworkError, StorageError, SystemError};
use crate::events::EventBus;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::services::auth_service::AuthService;
use crate::state::{
    AuthState, ChatState, MessageState, OfflineState, PresenceState, PrivacyState, StreamingState,
    UIState, UserState,
};
use crate::storage::{
    AttachmentRepository, ChannelRepository, Database, DraftRepository, KeyStore,
    MessageRepository, ReactionRepository, SettingsRepository, UserRepository,
};
use crate::system::{Directories, Keyring};
use crate::ui::bridge::UiBridge;

/// Default server URL if CHATTER_SERVER_URL is not set.
const DEFAULT_SERVER_URL: &str = "http://localhost:50051";

/// Environment variable for server URL.
const SERVER_URL_ENV: &str = "CHATTER_SERVER_URL";

/// Application context error.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// System initialization failed.
    #[error("system error: {0}")]
    System(#[from] SystemError),

    /// Storage initialization failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Network initialization failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),
}

/// Application context holding all initialized dependencies.
///
/// This is the central dependency container for the application. It initializes
/// all subsystems in the correct order and provides access to them.
///
/// # Initialization Order
///
/// 1. System: Directories, Keyring
/// 2. Storage: Database (with passphrase from keyring), KeyStore, Repositories
/// 3. Crypto: IdentityKeyManager, PreKeyManager, SessionManager, SenderKeyManager
/// 4. Network: GrpcClientPool, AuthInterceptor
/// 5. State: All state stores, EventBus
///
/// # Example
///
/// ```ignore
/// let ctx = AppContext::new()?;
///
/// // Get services
/// let auth_service = ctx.auth_service();
///
/// // Get UI bridge for Slint integration
/// let bridge = ctx.ui_bridge();
/// ```
pub struct AppContext {
    // System
    /// XDG-compliant directories.
    pub directories: Directories,
    /// System keyring for secure credential storage.
    pub keyring: Keyring,

    // Storage
    /// Encrypted SQLCipher database connection.
    pub database: Arc<Database>,
    /// Signal Protocol key storage.
    pub key_store: Arc<KeyStore>,

    // Repositories
    /// User and profile repository.
    pub user_repo: Arc<UserRepository>,
    /// Channel and membership repository.
    pub channel_repo: Arc<ChannelRepository>,
    /// Message repository with cursor pagination.
    pub message_repo: Arc<MessageRepository>,
    /// Application settings repository.
    pub settings_repo: Arc<SettingsRepository>,
    /// File attachment metadata repository.
    pub attachment_repo: Arc<AttachmentRepository>,
    /// Message reactions repository.
    pub reaction_repo: Arc<ReactionRepository>,
    /// Message draft repository.
    pub draft_repo: Arc<DraftRepository>,

    // Crypto managers
    /// Identity key management.
    pub identity_manager: Arc<IdentityKeyManager>,
    /// PreKey bundle management.
    pub prekey_manager: Arc<PreKeyManager>,
    /// Session management (stateless).
    pub session_manager: Arc<SessionManager>,
    /// Sender key management for groups (stateless).
    pub sender_key_manager: Arc<SenderKeyManager>,

    // Network
    /// gRPC connection pool.
    pub grpc_pool: Arc<GrpcClientPool>,
    /// Authentication interceptor for gRPC requests.
    pub auth_interceptor: AuthInterceptor,

    // State stores
    /// UI preferences state.
    pub ui_state: UIState,
    /// Authentication state.
    pub auth_state: AuthState,
    /// Chat view state.
    pub chat_state: ChatState,
    /// Message state with pagination.
    pub message_state: MessageState,
    /// User and friends state.
    pub user_state: UserState,
    /// Presence tracking state.
    pub presence_state: PresenceState,
    /// Privacy settings state.
    pub privacy_state: PrivacyState,
    /// Offline queue state.
    pub offline_state: OfflineState,
    /// Streaming aggregation state.
    pub streaming_state: StreamingState,
    /// Event bus for domain/UI events.
    pub event_bus: EventBus,
}

impl AppContext {
    /// Create a new AppContext, initializing all subsystems.
    ///
    /// This performs the full initialization sequence:
    /// 1. Creates XDG directories
    /// 2. Gets or generates database passphrase from keyring
    /// 3. Opens encrypted database and runs migrations
    /// 4. Creates all repositories
    /// 5. Creates crypto managers
    /// 6. Creates network pool
    /// 7. Creates state stores
    ///
    /// # Errors
    ///
    /// Returns `ContextError` if any initialization step fails.
    pub fn new() -> Result<Self, ContextError> {
        // 1. System initialization
        let directories = Directories::new()?;
        let keyring = Keyring::new();

        // 2. Database passphrase (get from keyring or generate)
        let passphrase = Self::passphrase_or_generate(&keyring)?;

        // 3. Open database with passphrase
        let db_path = directories.database_path();
        let database = Database::open(&db_path, &passphrase)?;

        // Run migrations
        database.run_migrations()?;

        let database = Arc::new(database);

        // 4. Create key store
        let key_store = Arc::new(KeyStore::new(database.clone()));

        // 5. Create repositories
        let user_repo = Arc::new(UserRepository::new(database.clone()));
        let channel_repo = Arc::new(ChannelRepository::new(database.clone()));
        let message_repo = Arc::new(MessageRepository::new(database.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
        let attachment_repo = Arc::new(AttachmentRepository::new(database.clone()));
        let reaction_repo = Arc::new(ReactionRepository::new(database.clone()));
        let draft_repo = Arc::new(DraftRepository::new(database.clone()));

        // 6. Create crypto managers
        let identity_manager = Arc::new(IdentityKeyManager::new(key_store.clone()));
        let prekey_manager = Arc::new(PreKeyManager::new(key_store.clone()));
        let session_manager = Arc::new(SessionManager);
        let sender_key_manager = Arc::new(SenderKeyManager::new());

        // 7. Create network pool
        let server_url = env::var(SERVER_URL_ENV).unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());
        let grpc_pool = Arc::new(GrpcClientPool::new(&server_url)?);
        let auth_interceptor = grpc_pool.auth_clone();

        // 8. Create state stores
        let ui_state = UIState::new();
        let auth_state = AuthState::new();
        let chat_state = ChatState::new();
        let message_state = MessageState::new();
        let user_state = UserState::new();
        let presence_state = PresenceState::new();
        let privacy_state = PrivacyState::new();
        let offline_state = OfflineState::new();
        let streaming_state = StreamingState::new();
        let event_bus = EventBus::new();

        Ok(Self {
            directories,
            keyring,
            database,
            key_store,
            user_repo,
            channel_repo,
            message_repo,
            settings_repo,
            attachment_repo,
            reaction_repo,
            draft_repo,
            identity_manager,
            prekey_manager,
            session_manager,
            sender_key_manager,
            grpc_pool,
            auth_interceptor,
            ui_state,
            auth_state,
            chat_state,
            message_state,
            user_state,
            presence_state,
            privacy_state,
            offline_state,
            streaming_state,
            event_bus,
        })
    }

    /// Get or generate database passphrase from keyring.
    ///
    /// If a passphrase exists in the keyring, returns it.
    /// Otherwise, generates a new 32-byte hex passphrase, stores it, and returns it.
    fn passphrase_or_generate(keyring: &Keyring) -> Result<String, ContextError> {
        // Try to get existing passphrase
        if let Ok(passphrase) = keyring.get_passphrase() {
            return Ok(passphrase);
        }

        // Generate new passphrase (32 random bytes as hex = 64 chars)
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
        let passphrase = hex::encode(bytes);

        // Store in keyring
        keyring.set_passphrase(&passphrase)?;

        Ok(passphrase)
    }

    /// Creates a UiBridge for Slint integration.
    ///
    /// The bridge provides access to UI and auth state stores and the event bus
    /// for wiring Slint callbacks and property synchronization.
    pub fn ui_bridge(&self) -> UiBridge {
        UiBridge::new(
            self.ui_state.clone(),
            self.auth_state.clone(),
            self.event_bus.clone(),
        )
    }

    /// Creates an AuthService with all required dependencies.
    ///
    /// The AuthService orchestrates login, registration, and logout flows.
    pub fn auth_service(&self) -> AuthService {
        AuthService::new(
            self.identity_manager.clone(),
            self.prekey_manager.clone(),
            self.user_repo.clone(),
            self.settings_repo.clone(),
            self.key_store.clone(),
            self.auth_interceptor.clone(),
        )
    }

    /// Creates an AuthClient connected to the server.
    ///
    /// # Panics
    ///
    /// Panics if grpc_pool is not connected.
    pub fn auth_client(&self) -> AuthClient {
        AuthClient::new(self.grpc_pool.channel(), self.auth_interceptor.clone())
    }

    /// Get the server URL being used.
    pub fn server_url(&self) -> String {
        env::var(SERVER_URL_ENV).unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_server_url() {
        assert_eq!(DEFAULT_SERVER_URL, "http://localhost:50051");
    }

    #[test]
    fn test_server_url_env_var() {
        assert_eq!(SERVER_URL_ENV, "CHATTER_SERVER_URL");
    }

    // Note: Full integration tests require a running keyring daemon
    // and are marked with #[ignore].

    #[test]
    #[ignore = "Requires running Secret Service"]
    fn test_app_context_new() {
        let ctx = AppContext::new();
        assert!(ctx.is_ok());
    }
}
