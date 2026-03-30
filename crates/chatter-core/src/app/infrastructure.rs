//! Infrastructure container.
//!
//! Low-level dependencies: storage, crypto, network. These are implementation
//! details that services depend on but UI should never access directly.

use std::env;
use std::sync::Arc;

use zeroize::Zeroize;

use crate::crypto::signal::{
    IdentityKeyManager, PreKeyManager, SesameManager, SqliteSesameStore,
};
use crate::mailbox::group::{GroupCoordinator, GroupMailboxManager};
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::network::streaming::{ReactionAggregator, TypingAggregator};
use crate::storage::repositories::{AnonDmRepository, AnonGroupRepository, JarvisGraphRepository, JarvisMemoryRepository, SectionRepository};
use crate::storage::{
    AttachmentRepository, ChannelRepository, Database, DraftRepository, KeyStore,
    MessageRepository, ReactionRepository, SearchIndex, SettingsRepository, UserRepository,
};
use crate::system::{Directories, Keyring};

use super::ContextError;

/// Default server URL if CHATTER_SERVER_URL is not set.
pub(crate) const DEFAULT_SERVER_URL: &str = "http://localhost:50051";

/// Environment variable for server URL.
pub(crate) const SERVER_URL_ENV: &str = "CHATTER_SERVER_URL";

/// Infrastructure container holding low-level dependencies.
///
/// Contains storage, crypto managers, and network pool. These are the foundation
/// that services build upon. UI code should never access these directly.
///
/// # Visibility
///
/// - `pub`: Items needed by ServiceContainer or post-boot initialization
/// - `pub(crate)`: Items that should not leak to UI layer
pub struct InfrastructureContainer {
    // System
    /// XDG-compliant directories.
    pub directories: Directories,
    /// System keyring for secure credential storage.
    pub keyring: Keyring,

    // Storage - pub for ServiceContainer and post-boot init
    /// Encrypted SQLCipher database connection.
    pub database: Arc<Database>,
    /// Signal Protocol key storage.
    pub key_store: Arc<KeyStore>,

    // Repositories - pub(crate) to hide from UI
    /// User and profile repository.
    pub(crate) user_repo: Arc<UserRepository>,
    /// Channel and membership repository.
    pub(crate) channel_repo: Arc<ChannelRepository>,
    /// Message repository with cursor pagination.
    pub(crate) message_repo: Arc<MessageRepository>,
    /// Application settings repository.
    pub(crate) settings_repo: Arc<SettingsRepository>,
    /// File attachment metadata repository.
    pub(crate) attachment_repo: Arc<AttachmentRepository>,
    /// Message reactions repository.
    pub(crate) reaction_repo: Arc<ReactionRepository>,
    /// Message draft repository.
    pub(crate) draft_repo: Arc<DraftRepository>,
    /// Full-text search index for messages.
    pub(crate) search_index: Arc<SearchIndex>,
    /// Anonymous group repository.
    pub(crate) anon_group_repo: Arc<AnonGroupRepository>,
    /// Anonymous DM repository.
    pub(crate) anon_dm_repo: Arc<AnonDmRepository>,
    /// Jarvis memory repository.
    pub(crate) jarvis_memory_repo: Arc<JarvisMemoryRepository>,
    /// Jarvis graph repository.
    pub(crate) jarvis_graph_repo: Arc<JarvisGraphRepository>,
    /// Custom sidebar section repository.
    pub(crate) section_repo: Arc<SectionRepository>,

    // Crypto managers - pub(crate)
    /// Identity key management.
    pub(crate) identity_manager: Arc<IdentityKeyManager>,
    /// PreKey bundle management.
    pub(crate) prekey_manager: Arc<PreKeyManager>,
    /// Sesame session management with message queueing.
    pub(crate) sesame_manager: Arc<SesameManager<SqliteSesameStore>>,

    // Streaming aggregators
    /// Reaction event aggregator for real-time reaction updates.
    pub(crate) reaction_aggregator: Arc<ReactionAggregator>,
    /// Typing indicator aggregator for real-time typing state.
    pub(crate) typing_aggregator: Arc<TypingAggregator>,

    // Network - pub for post-boot initialization
    /// gRPC connection pool.
    pub grpc_pool: Arc<GrpcClientPool>,
    /// Authentication interceptor for gRPC requests.
    pub auth_interceptor: AuthInterceptor,

    // Group infrastructure - pub for post-boot init
    /// Group mailbox state persistence.
    pub group_mailbox_manager: Arc<GroupMailboxManager>,
    /// Group mailbox coordinator for setup and announcements.
    pub group_coordinator: Arc<GroupCoordinator>,

    // Mailbox infrastructure
    /// Token manager for blind-signed anonymous mailbox and call room tokens.
    pub(crate) token_manager: Arc<TokenManager>,
    
    // Event infrastructure
    /// Event bus for domain events.
    pub(crate) event_bus: crate::events::EventBus,
    
    // Shared post-boot services (initialized later)
    /// Group fanout for multi-device messaging.
    pub group_fanout: Arc<std::sync::OnceLock<Arc<crate::mailbox::group::GroupFanout>>>,
}

impl InfrastructureContainer {
    /// Create a new InfrastructureContainer, initializing all low-level dependencies.
    ///
    /// This performs:
    /// 1. Creates XDG directories
    /// 2. Gets or generates database passphrase from keyring
    /// 3. Opens encrypted database and runs migrations
    /// 4. Creates all repositories
    /// 5. Creates crypto managers
    /// 6. Creates network pool
    ///
    /// # Errors
    ///
    /// Returns `ContextError` if any initialization step fails.
    pub fn new(data_dir: Option<String>) -> Result<Self, ContextError> {
        // 1. System initialization
        let directories = match data_dir {
            Some(dir) => Directories::with_base_dir(std::path::PathBuf::from(dir))?,
            None => Directories::new()?,
        };

        // 2. Get database path first (needed for keyring uniqueness)
        let db_path = directories.database_path();

        // 3. Create keyring tied to this database path
        let keyring = Keyring::new(db_path.clone());

        // 4. Database passphrase (get from keyring or generate)
        let mut passphrase = Self::passphrase_or_generate(&keyring, &directories)?;

        // 5. Open database with passphrase
        let database = Database::open(&db_path, &passphrase)?;

        // SECURITY: Zeroize passphrase immediately after use
        // This prevents it from lingering in memory
        passphrase.zeroize();

        // Run migrations
        database.run_migrations()?;

        // 5b. Set attachments base dir (critical for Android where dirs::data_local_dir is read-only)
        crate::services::attachment::set_attachments_base_dir(directories.data_dir().to_path_buf());

        // 5c. Ensure attachments directory exists for filesystem-based storage
        if let Err(e) = crate::services::attachment::ensure_attachments_dir() {
            tracing::warn!("Failed to create attachments directory: {}", e);
        }

        let database = Arc::new(database);

        // 6. Create key store
        let key_store = Arc::new(KeyStore::new(database.clone()));

        // 7. Create repositories
        let user_repo = Arc::new(UserRepository::new(database.clone()));
        let channel_repo = Arc::new(ChannelRepository::new(database.clone()));
        let message_repo = Arc::new(MessageRepository::new(database.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
        let attachment_repo = Arc::new(AttachmentRepository::new(database.clone()));
        let reaction_repo = Arc::new(ReactionRepository::new(database.clone()));
        let draft_repo = Arc::new(DraftRepository::new(database.clone()));
        let search_index = Arc::new(SearchIndex::new(database.clone()));
        let anon_group_repo = Arc::new(AnonGroupRepository::new(database.clone()));
        let anon_dm_repo = Arc::new(AnonDmRepository::new(database.clone()));
        let jarvis_memory_repo = Arc::new(JarvisMemoryRepository::new(database.clone()));
        let jarvis_graph_repo = Arc::new(JarvisGraphRepository::new(database.clone()));
        let section_repo = Arc::new(SectionRepository::new(database.clone()));

        // 8. Create crypto managers
        let identity_manager = Arc::new(IdentityKeyManager::new(key_store.clone()));
        let prekey_manager = Arc::new(PreKeyManager::new(key_store.clone()));

        // 8b. Create Sesame manager for multi-session handling and message queueing
        let sesame_store = SqliteSesameStore::new(database.clone());
        // Initialize Sesame tables (safe to call multiple times)
        if let Err(e) = sesame_store.init_tables() {
            tracing::warn!("Failed to initialize Sesame tables: {}", e);
        }
        let sesame_manager = Arc::new(SesameManager::new(sesame_store));

        // 9. Create network pool
        let server_url = env::var(SERVER_URL_ENV).unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());
        let grpc_pool = Arc::new(GrpcClientPool::new(&server_url)?);
        let auth_interceptor = grpc_pool.auth_clone();

        // 10. Create group mailbox infrastructure
        let group_mailbox_manager = Arc::new(GroupMailboxManager::new(database.clone()));
        let group_coordinator = Arc::new(GroupCoordinator::new(
            database.clone(),
            group_mailbox_manager.clone(),
        ));

        // 11. Create streaming aggregators
        let reaction_aggregator = Arc::new(ReactionAggregator::new());
        let typing_aggregator = Arc::new(TypingAggregator::new());

        // 12. Create token manager for anonymous mailbox and call room credentials
        let token_manager = Arc::new(TokenManager::new(database.clone()));

        tracing::info!("Infrastructure initialized");

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
            search_index,
            identity_manager,
            prekey_manager,
            sesame_manager,
            reaction_aggregator,
            typing_aggregator,
            grpc_pool,
            auth_interceptor,
            group_mailbox_manager,
            group_coordinator,
            token_manager,
            anon_group_repo,
            anon_dm_repo,
            jarvis_memory_repo,
            jarvis_graph_repo,
            section_repo,
            event_bus: crate::events::EventBus::new(),
            group_fanout: Arc::new(std::sync::OnceLock::new()),
        })
    }

    /// Get or generate database passphrase from keyring.
    ///
    /// On Android, the system keyring (secret-service/kwallet) is unavailable.
    /// Falls back to file-based storage in the app's private data directory,
    /// which is sandboxed by Android's per-app Linux UID.
    fn passphrase_or_generate(keyring: &Keyring, directories: &Directories) -> Result<String, ContextError> {
        // On Android, use file-based passphrase storage (app sandbox protects it)
        #[cfg(target_os = "android")]
        {
            let passphrase_file = directories.data_dir().join(".db_key");
            if let Ok(existing) = std::fs::read_to_string(&passphrase_file) {
                let trimmed = existing.trim().to_string();
                if !trimmed.is_empty() {
                    tracing::info!("Retrieved existing passphrase from app storage");
                    return Ok(trimmed);
                }
            }

            // Generate new passphrase
            let mut bytes = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
            let passphrase = hex::encode(bytes);

            // Store in app's private directory
            if let Some(parent) = passphrase_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&passphrase_file, &passphrase)
                .map_err(|e| ContextError::System(crate::error::SystemError::DirectoryCreate(e)))?;

            // Restrict permissions to owner only
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&passphrase_file, std::fs::Permissions::from_mode(0o600));
            }

            tracing::info!("Generated and stored new passphrase in app storage");
            return Ok(passphrase);
        }

        #[cfg(not(target_os = "android"))]
        {
            let _ = directories; // used only on Android
            const MAX_RETRIES: u32 = 6;
            const RETRY_DELAY_SECS: u64 = 5;

            // Try to get existing passphrase with retries
            match keyring.get_passphrase_with_retry(MAX_RETRIES, RETRY_DELAY_SECS) {
                Ok(passphrase) => {
                    tracing::info!("Retrieved existing passphrase from keyring");
                    return Ok(passphrase);
                }
                Err(e) => {
                    tracing::info!(
                        "No existing passphrase in keyring ({}), generating new one",
                        e
                    );
                }
            }

            // Generate new passphrase (32 random bytes as hex = 64 chars)
            let mut bytes = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
            let passphrase = hex::encode(bytes);

            // Store in keyring with retries
            tracing::info!("Please unlock your password manager to store the database passphrase...");
            keyring.set_passphrase_with_retry(&passphrase, MAX_RETRIES, RETRY_DELAY_SECS)?;

            // Verify it was stored correctly
            match keyring.get_passphrase() {
                Ok(_) => tracing::info!("Verified: passphrase successfully stored and retrievable"),
                Err(e) => tracing::error!("FAILED to verify passphrase storage: {}", e),
            }

            Ok(passphrase)
        }
    }

    /// Get the server URL being used.
    pub fn server_url(&self) -> String {
        env::var(SERVER_URL_ENV).unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string())
    }
}
