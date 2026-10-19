//! Authentication service.
//!
//! Login, logout, and session management orchestrating crypto, network, and storage layers.
//!
//! # Privacy
//!
//! The server sees a hash of the nickname (`auth_crypto::login_identifier`),
//! never the nickname, and never the password or anything password-equivalent:
//! every password check is an OPAQUE (RFC 9807) exchange (`crypto::opaque`).

use base64::Engine;
use rand::TryRngCore;
use std::sync::Arc;
use zeroize::Zeroize;

use crate::crypto::DeviceCredentials;
use crate::crypto::auth_crypto::{
    generate_profile_key, login_identifier, username_candidates, username_claim,
};
use crate::crypto::opaque::{self, OpaqueError};
use crate::crypto::signal::identity::IdentityKeyManager;
use crate::crypto::signal::prekey::PreKeyManager;
use crate::domain::ids::UserId;
use crate::domain::user::{PresenceStatus, Profile, User};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::services::key_recovery::KeyRecoveryNeed;
use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
use crate::storage::repositories::user_repo::UserRepository;

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Invalid username or password.
    #[error("invalid credentials")]
    InvalidCredentials,

    /// Registration failed.
    #[error("registration failed: {0}")]
    RegistrationFailed(String),

    /// Login failed for a reason other than bad credentials or the network.
    #[error("login failed: {0}")]
    LoginFailed(String),

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

    /// Caller-provided input violated a constraint (e.g. avatar too large).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The account's login guess ceiling is reached (server OPEN-ITEMS §133):
    /// no password login until it lapses. The ways in that do not use it are
    /// a recovery code and linking from a device that is already signed in.
    #[error("login guess limit reached")]
    LoginGuessLimit,
}

impl From<OpaqueError> for AuthError {
    fn from(e: OpaqueError) -> Self {
        match e {
            OpaqueError::InvalidLogin => AuthError::InvalidCredentials,
            OpaqueError::Protocol(m) => AuthError::Crypto(CryptoError::Protocol(m)),
        }
    }
}

impl AuthError {
    /// Text for the login/register screen: the `Display` chain
    /// ("login failed: ...") is for logs, not for people.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::InvalidCredentials => "Incorrect username or password.".to_string(),
            Self::LoginGuessLimit => "Too many password attempts on this account, so password sign-in \
                is paused for now. Sign in with a recovery code, or link this device from one that is \
                already signed in."
                .to_string(),
            Self::LoginFailed(m) | Self::RegistrationFailed(m) | Self::InvalidInput(m) => {
                let mut c = m.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            }
            other => other.to_string(),
        }
    }
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
    /// Whether identity keys were restored from an SVR backup during login.
    pub keys_restored: bool,
    /// Set when this device could not establish a Signal identity key.
    ///
    /// Replaces the old `keys_missing` / `backup_destroyed` booleans, which
    /// every caller in the tree ignored — the login looked successful, the UI
    /// dropped the result on the floor, and the user was never told their keys
    /// were gone. `ChatterClient::login` now refuses to return `Ok` while this
    /// is set, so the condition cannot be silently discarded again.
    pub key_recovery: Option<KeyRecoveryNeed>,
    /// One-time recovery codes returned during registration (shown once to user).
    pub recovery_codes: Option<Vec<String>>,
}

/// One-time codes retained in this device's encrypted settings until saved.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PendingRecoveryCodes {
    pub codes: Vec<String>,
    pub regenerated: bool,
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
    pub(crate) settings_repo: Arc<SettingsRepository>,
    key_store: Arc<KeyStore>,
    pub(crate) auth_interceptor: AuthInterceptor,
    /// `(login identifier, grant)` from the last recovery, spent by the next
    /// login for that identifier (server OPEN-ITEMS §133). Memory only: it
    /// lives 10 minutes on the server.
    recovery_login_grant: parking_lot::Mutex<Option<(String, Vec<u8>)>>,
}

mod connection;
mod login;
mod pow;
mod recovery;
mod register;
mod session;
mod unlink;

pub use pow::login_start_paying_pow;
pub(crate) use pow::prove_password;
pub use recovery::{RECOVERY_CODE_LEN, normalize_recovery_code};
pub(crate) use unlink::{unlinked_account, wipe_unlinked_install};
