//! Authentication service.
//!
//! Login, logout, and session management orchestrating crypto, network, and storage layers.

use rand::RngCore;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::crypto::signal::identity::IdentityKeyManager;
use crate::crypto::signal::prekey::PreKeyManager;
use crate::crypto::DeviceCredentials;
use crate::domain::ids::UserId;
use crate::domain::user::{PresenceStatus, Profile, User};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::storage::repositories::user_repo::UserRepository;
use crate::storage::KeyStore;

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Invalid username or password.
    #[error("invalid credentials")]
    InvalidCredentials,

    /// Registration failed.
    #[error("registration failed: {0}")]
    RegistrationFailed(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Cryptographic operation failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Operation requires user to be logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// User is already logged in.
    #[error("already logged in")]
    AlreadyLoggedIn,
}

/// Result of a successful login or registration.
#[derive(Debug, Clone)]
pub struct LoginResult {
    /// User ID returned from server.
    pub user_id: UserId,
    /// Device ID assigned to this device.
    pub device_id: u32,
    /// Display name of the user.
    pub display_name: String,
    /// Whether initial prekey upload is still needed.
    pub needs_prekey_upload: bool,
}

/// Authentication service.
///
/// Orchestrates login, registration, and logout flows by coordinating
/// crypto, network, and storage layers.
pub struct AuthService {
    grpc_pool: Arc<GrpcClientPool>,
    identity_manager: Arc<IdentityKeyManager>,
    prekey_manager: Arc<PreKeyManager>,
    user_repo: Arc<UserRepository>,
    settings_repo: Arc<SettingsRepository>,
    #[allow(dead_code)] // Reserved for future key management operations
    key_store: Arc<KeyStore>,
    auth_interceptor: AuthInterceptor,
}

impl AuthService {
    /// Create a new AuthService with all dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        identity_manager: Arc<IdentityKeyManager>,
        prekey_manager: Arc<PreKeyManager>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
        key_store: Arc<KeyStore>,
        auth_interceptor: AuthInterceptor,
    ) -> Self {
        Self {
            grpc_pool,
            identity_manager,
            prekey_manager,
            user_repo,
            settings_repo,
            key_store,
            auth_interceptor,
        }
    }

    /// Internal helper to create an auth client.
    fn auth_client(&self) -> AuthClient {
        AuthClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    /// Check if a user is currently logged in.
    pub fn is_logged_in(&self) -> bool {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .is_some()
    }

    /// Get the current user ID and device ID if logged in.
    pub fn current_user(&self) -> Option<(UserId, u32)> {
        let user_id_str = self.settings_repo.get_string(keys::CURRENT_USER_ID).ok()??;
        let device_id: u32 = self.settings_repo.get(keys::CURRENT_DEVICE_ID).ok()??;

        let user_id = user_id_str.parse().ok()?;
        Some((user_id, device_id))
    }

    /// Login with username and password.
    ///
    /// Orchestrates:
    /// 1. Password hashing
    /// 2. Identity key retrieval/generation
    /// 3. Device credential generation
    /// 4. Server authentication
    /// 5. Local state persistence
    /// 6. Auth interceptor configuration
    pub async fn login(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        device_info: &str,
    ) -> Result<LoginResult, AuthError> {
        // Check not already logged in
        if self.is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn);
        }

        // Hash password with SHA256 (server expects pre-hashed)
        let password_hash = Sha256::digest(password.as_bytes());

        // Get or generate identity key pair
        let (_registration_id, identity_key_pair) = if self.identity_manager.has_identity()? {
            self.identity_manager
                .get_identity()?
                .ok_or(CryptoError::NoIdentity)?
        } else {
            // Generate new identity for this device
            self.identity_manager.generate_identity(&mut rand::rng())?
        };

        // Generate profile key for UAK derivation
        let mut profile_key = [0u8; 32];
        rand::rng().fill_bytes(&mut profile_key);

        // For login, device_id might come from settings (re-auth) or be assigned by server
        let stored_device_id: Option<u32> = self.settings_repo.get(keys::CURRENT_DEVICE_ID).ok().flatten();
        let device_id_for_creds = stored_device_id.unwrap_or(1);

        // Generate device credentials
        let credentials = DeviceCredentials::generate(device_id_for_creds, &profile_key);

        // Call server login
        let identity_public_key = identity_key_pair.identity_key().serialize();
        let response = auth_client
            .login(
                username,
                password_hash.as_slice(),
                device_info,
                identity_public_key.as_ref(),
                credentials.auth_token().as_bytes(),
                credentials.unidentified_access_key(),
                stored_device_id,
            )
            .await
            .map_err(|e| match &e {
                NetworkError::Unauthenticated => AuthError::InvalidCredentials,
                _ => AuthError::Network(e),
            })?;

        // Extract session and user profile
        let session = response.session.ok_or_else(|| {
            AuthError::RegistrationFailed("no session returned".to_string())
        })?;
        let user_profile = response.user_profile.ok_or_else(|| {
            AuthError::RegistrationFailed("no user_profile returned".to_string())
        })?;

        let user_id: UserId = user_profile.user_id.parse().map_err(|_| {
            AuthError::RegistrationFailed("invalid user_id format".to_string())
        })?;
        let device_id = session.device_id;
        let display_name = user_profile.display_name.clone();

        // Persist auth state in settings
        self.settings_repo.set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo.set(keys::CURRENT_DEVICE_ID, &device_id)?;

        // Configure auth interceptor for Basic auth
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        // Store user locally if not exists
        if self.user_repo.get(&user_id)?.is_none() {
            let user = User {
                id: user_id,
                username: username.to_string(),
                svr_public_key: Vec::new(), // SVR key will be set during PIN setup
                last_active: chrono::Utc::now(),
                created_at: chrono::Utc::now(),
            };
            self.user_repo.insert(&user)?;
        }

        // Create/update profile
        let profile = Profile {
            user_id,
            display_name: display_name.clone(),
            avatar_url: if user_profile.avatar_url.is_empty() {
                None
            } else {
                Some(user_profile.avatar_url.clone())
            },
            status_message: None,
            presence: PresenceStatus::Online,
        };
        self.user_repo.upsert_profile(&profile)?;

        Ok(LoginResult {
            user_id,
            device_id,
            display_name,
            needs_prekey_upload: stored_device_id.is_none(), // New device needs prekey upload
        })
    }

    /// Register a new user account.
    ///
    /// Orchestrates:
    /// 1. Identity key generation
    /// 2. Prekey bundle generation
    /// 3. Device credential generation
    /// 4. Server registration
    /// 5. Local state persistence
    /// 6. Auth interceptor configuration
    pub async fn register(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        display_name: &str,
        device_info: &str,
    ) -> Result<LoginResult, AuthError> {
        // Check not already logged in
        if self.is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn);
        }

        // Generate new identity (always fresh for registration)
        let (registration_id, identity_key_pair) =
            self.identity_manager.generate_identity(&mut rand::rng())?;

        // Generate signed prekey
        let signed_prekey = self
            .prekey_manager
            .generate_signed_prekey(&mut rand::rng(), 1, &identity_key_pair)?;

        // Generate Kyber prekey for post-quantum resistance
        let kyber_prekey = self
            .prekey_manager
            .generate_kyber_prekey(1, &identity_key_pair)?;

        // Generate one-time prekeys (100 for initial batch)
        let one_time_prekeys = self.prekey_manager.generate_prekeys(&mut rand::rng(), 1, 100)?;

        // Generate profile key for UAK derivation
        let mut profile_key = [0u8; 32];
        rand::rng().fill_bytes(&mut profile_key);

        // Device ID is 1 for primary device during registration
        let device_id = 1u32;
        let credentials = DeviceCredentials::generate(device_id, &profile_key);

        // Hash password with SHA256
        let password_hash = Sha256::digest(password.as_bytes());

        // Build Signal Protocol prekey bundle for server
        use crate::proto::chat_v1::signal_pre_key_bundle::{
            KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord as ProtoPreKey,
            SignedPreKeyRecord as ProtoSignedPreKey,
        };
        use crate::proto::chat_v1::SignalPreKeyBundle;
        use libsignal_protocol::GenericSignedPreKey;

        let identity_public_key = identity_key_pair.identity_key().serialize();

        // Extract values from signed prekey (these are infallible for freshly generated keys)
        let signed_prekey_id: u32 = signed_prekey.id().map_err(|e| CryptoError::Protocol(e.to_string()))?.into();
        let signed_prekey_public = signed_prekey.public_key().map_err(|e| CryptoError::Protocol(e.to_string()))?.serialize();
        let signed_prekey_sig = signed_prekey.signature().map_err(|e| CryptoError::Protocol(e.to_string()))?;
        let signed_prekey_ts = signed_prekey.timestamp().map_err(|e| CryptoError::Protocol(e.to_string()))?.epoch_millis();

        // Extract Kyber prekey values
        let kyber_id: u32 = kyber_prekey.id().map_err(|e| CryptoError::Protocol(e.to_string()))?.into();
        let kyber_public = kyber_prekey.public_key().map_err(|e| CryptoError::Protocol(e.to_string()))?.serialize();
        let kyber_sig = kyber_prekey.signature().map_err(|e| CryptoError::Protocol(e.to_string()))?;

        let prekey_bundle = SignalPreKeyBundle {
            identity_key: identity_public_key.as_ref().to_vec(),
            registration_id,
            device_id,
            signed_prekey: Some(ProtoSignedPreKey {
                id: signed_prekey_id,
                public_key: signed_prekey_public.to_vec(),
                signature: signed_prekey_sig,
                timestamp: signed_prekey_ts,
            }),
            one_time_prekey: if let Some(pk) = one_time_prekeys.first() {
                let pk_id: u32 = pk.id().map_err(|e| CryptoError::Protocol(e.to_string()))?.into();
                let pk_public = pk.public_key().map_err(|e| CryptoError::Protocol(e.to_string()))?.serialize();
                Some(ProtoPreKey {
                    id: pk_id,
                    public_key: pk_public.to_vec(),
                })
            } else {
                None
            },
            kyber_prekey: Some(ProtoKyberPreKey {
                id: kyber_id,
                public_key: kyber_public.to_vec(),
                signature: kyber_sig,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("Time should move forward")
                    .as_millis() as u64,
            }),
            unidentified_access_key: None, // Server derives from profile_key
            // Per NDSS '21: Do NOT enable unrestricted access by default.
            // Initial contact uses anonymous credentials (blind-signed tokens) instead.
            // This prevents spam while preserving sender anonymity.
            unrestricted_unidentified_access: Some(false),
        };

        // Build initial one-time prekeys for server (skip first, it's in bundle)
        let mut initial_prekeys: Vec<ProtoPreKey> = Vec::new();
        for pk in one_time_prekeys.iter().skip(1) {
            let pk_id: u32 = pk.id().map_err(|e| CryptoError::Protocol(e.to_string()))?.into();
            let pk_public = pk.public_key().map_err(|e| CryptoError::Protocol(e.to_string()))?.serialize();
            initial_prekeys.push(ProtoPreKey {
                id: pk_id,
                public_key: pk_public.to_vec(),
            });
        }

        // Call server registration (wait for completion)
        // Send profile_key - server derives UAK from it
        let result = auth_client
            .register_and_wait(
                username,
                password_hash.as_slice(),
                display_name,
                identity_public_key.as_ref(),
                device_info,
                credentials.auth_token().as_bytes(),
                &profile_key,
                Some(prekey_bundle),
                initial_prekeys,
            )
            .await
            .map_err(|e| AuthError::RegistrationFailed(e.to_string()))?;

        // Extract user_id from registration result (server-assigned)
        let user_id: UserId = result.user_id.parse().map_err(|_| {
            AuthError::RegistrationFailed("invalid user_id format".to_string())
        })?;

        // Persist auth state (device_id is server-assigned)
        self.settings_repo.set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo.set(keys::CURRENT_DEVICE_ID, &result.device_id)?;

        // Configure auth interceptor for Basic auth
        // Client already has device_auth_token and UAK (generated locally)
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        // Store user locally
        let user = User {
            id: user_id,
            username: username.to_string(),
            svr_public_key: Vec::new(), // SVR key will be set during PIN setup
            last_active: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
        };
        self.user_repo.insert(&user)?;

        // Create profile from server response or use provided display_name
        let server_display_name = result
            .user_profile
            .as_ref()
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| display_name.to_string());
        let avatar_url = result.user_profile.as_ref().and_then(|p| {
            if p.avatar_url.is_empty() {
                None
            } else {
                Some(p.avatar_url.clone())
            }
        });

        let profile = Profile {
            user_id,
            display_name: server_display_name.clone(),
            avatar_url,
            status_message: None,
            presence: PresenceStatus::Online,
        };
        self.user_repo.upsert_profile(&profile)?;

        Ok(LoginResult {
            user_id,
            device_id: result.device_id,
            display_name: server_display_name,
            needs_prekey_upload: false, // Prekeys already generated (upload them next)
        })
    }

    /// Logout the current user.
    ///
    /// Orchestrates:
    /// 1. Server logout notification
    /// 2. Auth interceptor clearing
    /// 3. Local auth state clearing (keeps identity keys for re-login)
    pub async fn logout(&self, auth_client: &mut AuthClient) -> Result<(), AuthError> {
        // Check is logged in
        if !self.is_logged_in() {
            return Err(AuthError::NotLoggedIn);
        }

        // Get current device ID
        let device_id: u32 = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)?
            .unwrap_or(0);

        // Notify server of logout (ignore errors - we logout locally regardless)
        let _ = auth_client.logout(device_id).await;

        // Clear auth interceptor
        self.auth_interceptor.clear();

        // Clear auth state from settings (but keep identity keys)
        self.settings_repo.delete(keys::CURRENT_USER_ID)?;
        self.settings_repo.delete(keys::CURRENT_DEVICE_ID)?;

        Ok(())
    }

    // ==================== Simplified Wrappers ====================

    /// Simplified wrapper for login that creates the client internally.
    pub async fn do_login(
        &self,
        username: &str,
        password: &str,
        device_name: &str,
    ) -> Result<LoginResult, AuthError> {
        let mut auth_client = self.auth_client();
        self.login(&mut auth_client, username, password, device_name)
            .await
    }

    /// Simplified wrapper for register that creates the client internally.
    pub async fn do_register(
        &self,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
    ) -> Result<LoginResult, AuthError> {
        let mut auth_client = self.auth_client();
        self.register(&mut auth_client, username, password, display_name, device_name)
            .await
    }

    /// Simplified wrapper for logout that creates the client internally.
    pub async fn do_logout(&self) -> Result<(), AuthError> {
        let mut auth_client = self.auth_client();
        self.logout(&mut auth_client).await
    }
}
