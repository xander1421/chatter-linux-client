//! Infrastructure container.
//!
//! Low-level dependencies: storage, crypto, network. These are implementation
//! details that services depend on but UI should never access directly.

use std::sync::Arc;

use zeroize::Zeroize;

use crate::crypto::signal::{IdentityKeyManager, PreKeyManager};
use crate::mailbox::group::{GroupCoordinator, GroupMailboxManager};
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::storage::repositories::{
    AnonDmRepository, AnonGroupRepository, EphemeralRepository, PendingDmRepository,
    SectionRepository, UnifiedConversationRepository, VectorRepository,
};
#[cfg(feature = "jarvis")]
use crate::storage::repositories::{JarvisGraphRepository, JarvisMemoryRepository};
use crate::storage::{
    AttachmentRepository, ChannelRepository, Database, KeyStore, MessageRepository, SearchIndex,
    SettingsRepository, UserRepository,
};
use crate::system::{Directories, Keyring};

use super::ContextError;

pub(crate) use super::server_endpoint::resolve_server_url;

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
    /// Full-text search index for messages.
    pub(crate) search_index: Arc<SearchIndex>,
    /// Anonymous group repository.
    pub(crate) anon_group_repo: Arc<AnonGroupRepository>,
    /// Anonymous DM repository.
    pub(crate) anon_dm_repo: Arc<AnonDmRepository>,
    /// Ephemeral (disappearing-message) TTL tracking repository.
    pub(crate) ephemeral_repo: Arc<EphemeralRepository>,
    /// Unified conversation repository.
    pub(crate) unified_conversation_repo: Arc<UnifiedConversationRepository>,
    /// Jarvis memory repository.
    #[cfg(feature = "jarvis")]
    pub(crate) jarvis_memory_repo: Arc<JarvisMemoryRepository>,
    /// Jarvis graph repository.
    #[cfg(feature = "jarvis")]
    pub(crate) jarvis_graph_repo: Arc<JarvisGraphRepository>,
    /// Custom sidebar section repository.
    pub(crate) section_repo: Arc<SectionRepository>,
    /// Vector embedding repository for semantic search.
    pub(crate) vector_repo: Arc<VectorRepository>,
    /// DMs held back until a first-contact bootstrap completes.
    pub(crate) pending_dm_repo: Arc<PendingDmRepository>,

    // Crypto managers - pub(crate)
    /// Identity key management.
    pub(crate) identity_manager: Arc<IdentityKeyManager>,
    /// PreKey bundle management.
    pub(crate) prekey_manager: Arc<PreKeyManager>,

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
    /// Token manager for blind-signed anonymous mailbox tokens.
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
    pub fn new(data_dir: Option<String>, server_url: &str) -> Result<Self, ContextError> {
        // 1. System initialization
        let directories = match data_dir {
            Some(dir) => Directories::with_base_dir(std::path::PathBuf::from(dir))?,
            None => Directories::new()?,
        };

        // 2. Get database path first (needed for keyring uniqueness)
        let db_path = directories.database_path();

        // Arti keeps its state and directory cache inside the app's own
        // directories (Android has no $HOME/XDG for its defaults).
        #[cfg(feature = "onion")]
        crate::network::grpc::onion_transport::set_tor_directories(
            directories.data_dir().join("tor"),
            directories.cache_dir().join("tor"),
        );

        // 3. Create keyring tied to this database path
        let keyring = Keyring::new(db_path.clone());

        // 4. Database passphrase (get from keyring or generate)
        let mut passphrase = Self::passphrase_or_generate(&keyring, &directories)?;

        // 5. Open database with passphrase.
        //
        // If the passphrase file was wiped but the encrypted DB survived
        // (Android partial reinstall, keyring loss, etc.), SQLCipher
        // returns "Invalid encryption key: file is not a database". That
        // state is unrecoverable — the key material is gone — so treat
        // the DB as dead, remove it, and retry with a fresh passphrase
        // rather than bricking account creation forever.
        let database = match Database::open(&db_path, &passphrase) {
            Ok(db) => db,
            Err(crate::error::StorageError::Encryption(msg)) => {
                tracing::warn!(
                    error = %msg,
                    "Database passphrase mismatch — wiping encrypted DB and retrying with fresh key"
                );
                passphrase.zeroize();
                // Move the old database aside rather than deleting it.
                //
                // The recovery path is unchanged — a fresh database is created
                // and the app starts — but the bytes survive. The comment above
                // assumes the only way here is genuine key loss, and that is not
                // true: anything that yields the wrong passphrase lands here,
                // and in most of those cases the real key still exists
                // somewhere. Deleting made that unrecoverable; renaming does not,
                // and costs one stale file.
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for p in [
                    &db_path,
                    &db_path.with_extension("db-wal"),
                    &db_path.with_extension("db-shm"),
                ] {
                    if !p.exists() {
                        continue;
                    }
                    let mut aside = p.clone().into_os_string();
                    aside.push(format!(".unreadable-{stamp}"));
                    let aside = std::path::PathBuf::from(aside);
                    if let Err(error) = std::fs::rename(p, &aside) {
                        tracing::error!(
                            path = %p.display(),
                            %error,
                            "could not move the unreadable database aside; leaving it untouched"
                        );
                    } else {
                        tracing::warn!(
                            from = %p.display(),
                            to = %aside.display(),
                            "database could not be decrypted; kept a copy rather than deleting it"
                        );
                    }
                }
                #[cfg(target_os = "android")]
                {
                    let _ = std::fs::remove_file(directories.data_dir().join(".db_key"));
                }
                passphrase = Self::passphrase_or_generate(&keyring, &directories)?;
                Database::open(&db_path, &passphrase)?
            }
            Err(e) => return Err(e.into()),
        };

        // SECURITY: Zeroize passphrase immediately after use
        // This prevents it from lingering in memory
        passphrase.zeroize();

        // Run migrations
        database.run_migrations()?;

        // 5b. Set attachments base dir (critical for Android where the platform data-local dir is read-only)
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
        let search_index = Arc::new(SearchIndex::new(database.clone()));
        let anon_group_repo = Arc::new(AnonGroupRepository::new(database.clone()));
        let anon_dm_repo = Arc::new(AnonDmRepository::new(database.clone()));
        let ephemeral_repo = Arc::new(EphemeralRepository::new(database.clone()));
        let unified_conversation_repo =
            Arc::new(UnifiedConversationRepository::new(database.clone()));
        #[cfg(feature = "jarvis")]
        let jarvis_memory_repo = Arc::new(JarvisMemoryRepository::new(database.clone()));
        #[cfg(feature = "jarvis")]
        let jarvis_graph_repo = Arc::new(JarvisGraphRepository::new(database.clone()));
        let section_repo = Arc::new(SectionRepository::new(database.clone()));
        let vector_repo = Arc::new(VectorRepository::new(database.clone()));
        let pending_dm_repo = Arc::new(PendingDmRepository::new(database.clone()));

        // 8. Create crypto managers
        let identity_manager = Arc::new(IdentityKeyManager::new(key_store.clone()));
        let prekey_manager = Arc::new(PreKeyManager::new(key_store.clone()));

        // 9. Create network pool.
        // No request timeout: this one pool serves long-lived server-streams
        // (notifications, channel events, DM/user events) alongside unary RPCs.
        // A channel-wide 30s timeout cancels those streams with "Timeout
        // expired"; HTTP/2 keepalive (30s/10s, while-idle) covers dead-peer
        // detection. Add a per-call `Request::set_timeout` if a unary ever hangs.
        let grpc_pool = Arc::new(GrpcClientPool::new_for_streaming(server_url)?);
        let auth_interceptor = grpc_pool.auth_clone();

        // The build's pinned .onion (config::PINNED_ONION_ADDR), so anon mode can
        // route the pool over Tor. Normalize a bare "<hash>.onion" into
        // "http://<hash>.onion:443".
        if let Some(onion) = crate::config::PINNED_ONION_ADDR {
            let onion = onion.trim();
            if !onion.is_empty() {
                let url = if onion.contains("://") {
                    onion.to_string()
                } else {
                    format!("http://{onion}:443")
                };
                grpc_pool.set_onion_address(Some(url));
                tracing::info!("pinned server .onion for anon-mode Tor transport");
            }
        }

        // 10. Create group mailbox infrastructure
        let group_mailbox_manager = Arc::new(GroupMailboxManager::new(database.clone()));
        let group_coordinator = Arc::new(GroupCoordinator::new(
            database.clone(),
            group_mailbox_manager.clone(),
        ));

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
            search_index,
            identity_manager,
            prekey_manager,
            pending_dm_repo,
            grpc_pool,
            auth_interceptor,
            group_mailbox_manager,
            group_coordinator,
            token_manager,
            anon_group_repo,
            anon_dm_repo,
            ephemeral_repo,
            unified_conversation_repo,
            #[cfg(feature = "jarvis")]
            jarvis_memory_repo,
            #[cfg(feature = "jarvis")]
            jarvis_graph_repo,
            section_repo,
            vector_repo,
            event_bus: crate::events::EventBus::new(),
            group_fanout: Arc::new(std::sync::OnceLock::new()),
        })
    }

    /// The Android `.db_key` logic, with the Keystore wrapper as a parameter so
    /// it is testable on the host.
    ///
    /// Generates ONLY when there is neither a key file nor a database. A sealed
    /// key the Keystore will not open, or a database whose key file is gone,
    /// is an error: generating there would orphan the existing database (the
    /// caller then moves it aside as unreadable).
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    fn android_passphrase(
        directories: &Directories,
        wrap: &dyn crate::system::sealed_secret::KeyWrap,
    ) -> Result<String, ContextError> {
        use crate::system::sealed_secret;
        let err = |e: String| ContextError::System(crate::error::SystemError::Keyring(e));
        let key_file = directories.data_dir().join(".db_key");

        if let Some(existing) = sealed_secret::read(&key_file, sealed_secret::DB_ALIAS, wrap)
            .map_err(|e| {
                err(format!(
                    "{e}. The database is left untouched; the app cannot open it on this device. \
                 Reinstall (clearing app data) to start over."
                ))
            })?
        {
            tracing::info!("unsealed the database passphrase with the Android Keystore");
            return Ok(existing.to_string());
        }

        if directories.database_path().exists() {
            return Err(err(format!(
                "{} exists but its key file {} is missing, so it cannot be opened. \
                 Refusing to generate a new key over it; reinstall (clearing app data) to start over.",
                directories.database_path().display(),
                key_file.display()
            )));
        }

        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        // OsRng for the DB master passphrase (highest-value key) — matches
        // the crate's stated RNG policy, not the ThreadRng userspace CSPRNG.
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(bytes.as_mut())
                .map_err(|e| err(format!("OS CSPRNG failed: {e}")))?;
        }
        let passphrase = zeroize::Zeroizing::new(hex::encode(bytes.as_ref()));
        // Persisted before use: a database keyed by a passphrase we failed to
        // store would be unopenable on the next launch.
        sealed_secret::write(&key_file, sealed_secret::DB_ALIAS, wrap, &passphrase).map_err(err)?;
        tracing::info!("generated a database passphrase and sealed it with the Android Keystore");
        Ok(passphrase.to_string())
    }

    /// Get or generate database passphrase from keyring.
    ///
    /// On Android, the system keyring (secret-service/kwallet) is unavailable.
    /// The passphrase lives in `.db_key` in the app data directory, sealed
    /// under an Android Keystore key (`system::sealed_secret`), so a copy of
    /// the app's files does not contain it.
    fn passphrase_or_generate(
        keyring: &Keyring,
        directories: &Directories,
    ) -> Result<String, ContextError> {
        #[cfg(target_os = "android")]
        {
            let _ = keyring;
            let wrap = crate::system::sealed_secret::installed_key_wrap()
                .map_err(|e| ContextError::System(crate::error::SystemError::Keyring(e)))?;
            return Self::android_passphrase(directories, wrap);
        }

        #[cfg(not(target_os = "android"))]
        {
            let _ = directories; // used only on Android
            const MAX_RETRIES: u32 = 6;
            const RETRY_DELAY_SECS: u64 = 5;

            // Try to get existing passphrase with retries
            match keyring.get_passphrase_with_retry(MAX_RETRIES, RETRY_DELAY_SECS) {
                Ok(passphrase) => {
                    tracing::info!(
                        source = keyring.passphrase_source(),
                        "retrieved the existing database passphrase"
                    );
                    return Ok(passphrase);
                }
                // ONLY a genuine absence may generate a new passphrase.
                //
                // Any other failure — a locked password manager, a timeout, a
                // denied request — means the passphrase very probably still
                // exists and we simply could not read it. Generating a fresh one
                // then fails to open the existing database, and the caller
                // treats that failure as an unrecoverable database and DELETES
                // it. A locked KeePass must not be able to destroy the user's
                // messages.
                Err(crate::error::SystemError::KeyringNoEntry) => {
                    tracing::info!("No passphrase in keyring; generating one for a first run");
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "Could not read the database passphrase. Refusing to continue: \
                         generating a new one here would make the existing database \
                         undecryptable and it would then be deleted as unrecoverable. \
                         Unlock your keyring and try again, or set CHATTER_DB_PASSPHRASE."
                    );
                    return Err(e.into());
                }
            }

            // Generate new passphrase (32 random bytes as hex = 64 chars)
            let mut bytes = [0u8; 32];
            // OsRng for the DB master passphrase (highest-value key) — matches
            // the crate's stated RNG policy, not the ThreadRng userspace CSPRNG.
            {
                use rand::TryRngCore;
                rand::rngs::OsRng
                    .try_fill_bytes(&mut bytes)
                    .expect("OS CSPRNG must not fail");
            }
            let passphrase = hex::encode(bytes);

            // Store in keyring with retries
            tracing::info!(
                "Please unlock your password manager to store the database passphrase..."
            );
            keyring.set_passphrase_with_retry(&passphrase, MAX_RETRIES, RETRY_DELAY_SECS)?;

            // Verify it was stored correctly
            match keyring.get_passphrase() {
                Ok(_) => tracing::info!("Verified: passphrase successfully stored and retrievable"),
                Err(e) => tracing::error!("FAILED to verify passphrase storage: {}", e),
            }

            Ok(passphrase)
        }
    }
}

#[cfg(test)]
mod android_passphrase_tests {
    use super::*;
    use crate::system::sealed_secret::tests::FakeWrap;

    const OK: FakeWrap = FakeWrap { fail_unseal: false };

    fn dirs() -> (tempfile::TempDir, Directories) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Directories::with_base_dir(tmp.path().to_path_buf()).unwrap();
        (tmp, dirs)
    }

    #[test]
    fn first_run_generates_seals_and_then_reads_back_the_same_passphrase() {
        let (_tmp, dirs) = dirs();
        let first = InfrastructureContainer::android_passphrase(&dirs, &OK).unwrap();
        assert_eq!(first.len(), 64);
        let on_disk = std::fs::read_to_string(dirs.data_dir().join(".db_key")).unwrap();
        assert!(on_disk.starts_with("ks1:") && !on_disk.contains(&first));
        std::fs::write(dirs.database_path(), b"db").unwrap();
        assert_eq!(
            InfrastructureContainer::android_passphrase(&dirs, &OK).unwrap(),
            first
        );
    }

    #[test]
    fn an_unsealable_key_fails_closed_and_leaves_key_and_database_alone() {
        let (_tmp, dirs) = dirs();
        InfrastructureContainer::android_passphrase(&dirs, &OK).unwrap();
        std::fs::write(dirs.database_path(), b"db").unwrap();
        let key_before = std::fs::read(dirs.data_dir().join(".db_key")).unwrap();
        assert!(
            InfrastructureContainer::android_passphrase(&dirs, &FakeWrap { fail_unseal: true })
                .is_err()
        );
        assert_eq!(
            std::fs::read(dirs.data_dir().join(".db_key")).unwrap(),
            key_before
        );
        assert_eq!(std::fs::read(dirs.database_path()).unwrap(), b"db");
    }

    #[test]
    fn a_database_without_a_key_file_is_refused_not_rekeyed() {
        let (_tmp, dirs) = dirs();
        std::fs::write(dirs.database_path(), b"db").unwrap();
        assert!(InfrastructureContainer::android_passphrase(&dirs, &OK).is_err());
        assert!(!dirs.data_dir().join(".db_key").exists());
    }
}
