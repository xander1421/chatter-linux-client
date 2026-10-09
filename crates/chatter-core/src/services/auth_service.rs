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

/// The server's stable token for [`AuthError::LoginGuessLimit`]
/// (auth.proto `LoginStartResponse`), carried in a FAILED_PRECONDITION.
const LOGIN_GUESS_LIMIT_TOKEN: &str = "login_guess_limit:";

/// The server's stable token for a trusted-device token it no longer honours
/// (password changed, recovered, expired), carried in a FAILED_PRECONDITION.
const DEVICE_TOKEN_INVALID_TOKEN: &str = "device_token_invalid:";

/// How often a solved login challenge is resent when the server says its
/// verification budget is momentarily spent (RESOURCE_EXHAUSTED "pow_busy:"),
/// and how long to wait between. The challenge is not consumed by that
/// refusal, so resending the same solution is all it takes.
const POW_BUSY_RETRIES: usize = 3;
const POW_BUSY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1500);

fn login_start_error(e: NetworkError) -> AuthError {
    match &e {
        NetworkError::InvalidRequest(m) if m.contains(LOGIN_GUESS_LIMIT_TOKEN) => {
            AuthError::LoginGuessLimit
        }
        _ => AuthError::Network(e),
    }
}

impl From<OpaqueError> for AuthError {
    fn from(e: OpaqueError) -> Self {
        match e {
            OpaqueError::InvalidLogin => AuthError::InvalidCredentials,
            OpaqueError::Protocol(m) => AuthError::Crypto(CryptoError::Protocol(m)),
        }
    }
}

/// Prove the current password to the server for the authenticated account
/// (OPAQUE KE1 -> KE2 -> KE3 against its own record). The returned proof is
/// single-use and is spent by the RPC it rides in (RegenerateRecoveryCodes,
/// DeleteAccount, VerifyPassword). A wrong password fails HERE, client-side,
/// with [`AuthError::InvalidCredentials`]: KE2 does not open, so there is no
/// KE3 to send.
pub(crate) async fn prove_password(
    auth_client: &mut AuthClient,
    password: &str,
) -> Result<crate::proto::chat_v1::PasswordProof, AuthError> {
    let start = opaque::login_start(password)?;
    let opened = auth_client.password_proof_start(&start.ke1).await?;
    let done = opaque::login_finish_async(start, password, opened.ke2).await?;
    Ok(crate::proto::chat_v1::PasswordProof {
        proof_id: opened.proof_id,
        ke3: done.ke3,
    })
}

/// How long a login proof of work may run. The server keeps the challenge
/// 3 minutes; the cap difficulty (9 bits = 512 expected Argon2id
/// evaluations) is ESTIMATED at mean ~5 s, p90 ~12 s on a Galaxy S9 (server
/// `login_pow.go` `LoginPoWCapBits`; not measured on the device).
const LOGIN_POW_DEADLINE: std::time::Duration = std::time::Duration::from_secs(150);

/// Most proofs of work one login pays. The server can ask twice: under onion
/// load it prices the first attempt at the transport's effort without looking
/// at the account, and the account's own price (or a price that rose while
/// solving) comes back as a second challenge. A third means something is
/// wrong; give up rather than spin.
const LOGIN_POW_ROUNDS: usize = 3;

/// OPAQUE LoginStart, paying the server's price when it names one (server
/// OPEN-ITEMS §133, §134). A `pow_challenge` instead of a KE2 is solved off
/// the async runtime ([`pow::solve_async`]) and the call repeated with the
/// token and solution, up to [`LOGIN_POW_ROUNDS`] times. `on_verifying` fires
/// before the first solve so the caller can show that the login is working,
/// not stuck.
///
/// [`pow::solve_async`]: crate::crypto::pow::solve_async
///
/// `login_grant` is a recovery's grant (`UseRecoveryCodeFinish`): it skips the
/// price and the account's guess ceiling. A refused grant (expired, already
/// used) falls back to an ordinary attempt. The ceiling itself surfaces as
/// [`AuthError::LoginGuessLimit`].
///
/// `device_token` is this device's trusted-device token for `identifier`
/// (from its last login or registration): with it the server meters this
/// account's trusted reserve instead of the account's untrusted price and
/// ceiling, so a stranger burning the ceiling cannot keep it out. A token the
/// server no longer honours (password changed or recovered, expired) is set
/// to `None` here — the caller deletes its stored copy — and the login goes
/// on without it.
pub async fn login_start_paying_pow(
    auth_client: &mut AuthClient,
    identifier: &str,
    ke1: &[u8],
    login_grant: Option<&[u8]>,
    device_token: &mut Option<Vec<u8>>,
    on_verifying: &(dyn Fn() + Send + Sync),
) -> Result<crate::proto::chat_v1::LoginStartResponse, AuthError> {
    use crate::crypto::pow;
    if let Some(grant) = login_grant {
        match auth_client
            .login_start(identifier, ke1, None, grant, &[])
            .await
        {
            Ok(r) => return Ok(r),
            Err(NetworkError::PermissionDenied) => {
                tracing::warn!("recovery login grant refused; logging in without it");
            }
            Err(e) => return Err(login_start_error(e)),
        }
    }
    let mut response = login_start_once(auth_client, identifier, ke1, None, device_token).await?;
    let started = std::time::Instant::now();
    for round in 0..LOGIN_POW_ROUNDS {
        let Some(challenge) = response.pow_challenge.take() else {
            return Ok(response);
        };
        if round == 0 {
            on_verifying();
        }
        let solve_started = std::time::Instant::now();
        let solution = pow::solve_async(
            pow::Purpose::Login,
            challenge.token.clone(),
            challenge.difficulty,
            challenge.gate_bits,
            started + LOGIN_POW_DEADLINE,
        )
        .await?;
        tracing::info!(
            pow_bits = challenge.difficulty,
            gate_bits = challenge.gate_bits,
            solve_ms = solve_started.elapsed().as_millis() as u64,
            "login proof of work solved"
        );
        let mut busy = 0;
        response = loop {
            match login_start_once(
                auth_client,
                identifier,
                ke1,
                Some((&challenge.token, solution)),
                device_token,
            )
            .await
            {
                Err(AuthError::Network(NetworkError::RateLimited)) if busy < POW_BUSY_RETRIES => {
                    busy += 1;
                    tracing::info!(
                        attempt = busy,
                        "server busy verifying proof of work; resending"
                    );
                    tokio::time::sleep(POW_BUSY_BACKOFF).await;
                }
                other => break other?,
            }
        };
    }
    if response.pow_challenge.is_some() {
        return Err(AuthError::LoginFailed(
            "the server kept asking for proof of work; try again".to_string(),
        ));
    }
    Ok(response)
}

/// One LoginStart presenting `device_token` if there is one. A token the
/// server refuses as `device_token_invalid:` is dropped (set to `None`) and
/// login starts again without a solution: the server may already have spent
/// the paid challenge before checking that token. Reusing it would turn a
/// recoverable stale token into a rejected replay under load.
async fn login_start_once(
    auth_client: &mut AuthClient,
    identifier: &str,
    ke1: &[u8],
    pow: Option<(&[u8], u64)>,
    device_token: &mut Option<Vec<u8>>,
) -> Result<crate::proto::chat_v1::LoginStartResponse, AuthError> {
    if let Some(tok) = device_token.as_deref() {
        match auth_client
            .login_start(identifier, ke1, pow, &[], tok)
            .await
        {
            Err(NetworkError::InvalidRequest(m)) if m.contains(DEVICE_TOKEN_INVALID_TOKEN) => {
                tracing::warn!("trusted device token no longer valid; logging in without it");
                *device_token = None;
                return auth_client
                    .login_start(identifier, ke1, None, &[], &[])
                    .await
                    .map_err(login_start_error);
            }
            other => return other.map_err(login_start_error),
        }
    }
    auth_client
        .login_start(identifier, ke1, pow, &[], &[])
        .await
        .map_err(login_start_error)
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
            recovery_login_grant: parking_lot::Mutex::new(None),
        }
    }

    // ========================================================================
    // Connection management (delegates to grpc_pool)
    // ========================================================================

    /// Get the current server endpoint URL.
    pub fn endpoint(&self) -> String {
        self.grpc_pool.endpoint()
    }

    /// Update the server URL. Disconnects any existing connection.
    pub fn set_server_url(&self, url: &str) -> Result<(), NetworkError> {
        self.grpc_pool.set_url(url)
    }

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.grpc_pool.is_connected()
    }

    /// Connect to the server.
    pub async fn connect(&self) -> Result<(), NetworkError> {
        self.grpc_pool.connect().await
    }

    /// Disconnect from the server.
    pub fn disconnect(&self) {
        self.grpc_pool.disconnect();
    }

    /// Create an auth client for making gRPC calls.
    /// Returns None if not connected to server.
    pub fn try_auth_client(&self) -> Option<AuthClient> {
        self.grpc_pool
            .try_channel()
            .map(|channel| AuthClient::new(channel, self.grpc_pool.auth_clone()))
    }

    /// Check if a user is currently logged in.
    pub fn is_logged_in(&self) -> bool {
        if self
            .settings_repo
            .get_bytes(keys::DEVICE_AUTH_TOKEN)
            .ok()
            .flatten()
            .is_none()
        {
            return false;
        }
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .is_some()
    }

    /// Check if a user has ever registered on this device.
    ///
    /// Returns true if there's any user in the local database,
    /// indicating the user should see login screen instead of register.
    pub fn has_registered_before(&self) -> bool {
        // Check if there are any users stored locally
        match self.user_repo.count() {
            Ok(count) => count > 0,
            Err(_) => false,
        }
    }

    /// Get the current user ID and device ID if logged in.
    pub fn current_user(&self) -> Option<(UserId, u32)> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()??;
        let device_id: u32 = self.settings_repo.get(keys::CURRENT_DEVICE_ID).ok()??;

        let user_id = user_id_str.parse().ok()?;
        Some((user_id, device_id))
    }

    /// Restore the authentication session from saved credentials.
    ///
    /// Loads password-based (Basic auth) credentials from encrypted settings
    /// and configures the auth interceptor. This must be called on application
    /// startup.
    ///
    /// Returns true if a session was successfully restored.
    pub fn restore_session(&self) -> bool {
        self.restore_password_session()
    }

    /// Restore a password-based (Basic auth) session.
    fn restore_password_session(&self) -> bool {
        // Every miss below is logged. A silent `return false` here presents as
        // "registration worked but the app is logged out on next launch", with
        // nothing in the log to say which of the four keys was absent.
        macro_rules! need {
            ($key:expr, $get:expr) => {
                match $get {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        tracing::warn!("No saved session: {} is not set", $key);
                        return false;
                    }
                    Err(e) => {
                        tracing::warn!("No saved session: reading {} failed: {e}", $key);
                        return false;
                    }
                }
            };
        }

        let user_id_str = need!(
            keys::CURRENT_USER_ID,
            self.settings_repo.get_string(keys::CURRENT_USER_ID)
        );
        let device_id: u32 = need!(
            keys::CURRENT_DEVICE_ID,
            self.settings_repo.get(keys::CURRENT_DEVICE_ID)
        );
        let profile_key_bytes = need!(
            keys::PROFILE_KEY,
            self.settings_repo.get_bytes(keys::PROFILE_KEY)
        );
        let auth_token_bytes = need!(
            keys::DEVICE_AUTH_TOKEN,
            self.settings_repo.get_bytes(keys::DEVICE_AUTH_TOKEN)
        );

        if profile_key_bytes.len() != 32 || auth_token_bytes.len() != 32 {
            tracing::warn!("Saved auth credentials have invalid length");
            return false;
        }

        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&profile_key_bytes);

        let mut auth_token_arr = [0u8; 32];
        auth_token_arr.copy_from_slice(&auth_token_bytes);

        // Reconstruct credentials
        let uak = crate::crypto::device_credentials::derive_uak(&profile_key);
        let credentials = crate::crypto::device_credentials::DeviceCredentials::from_stored(
            device_id,
            auth_token_arr,
            uak,
        );

        // Configure auth interceptor
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id_str),
            credentials.auth_token_base64(),
        );

        tracing::info!(
            user_id = %user_id_str,
            device_id = %device_id,
            "Restored password authentication session"
        );

        true
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
    /// Get current identity from the server.
    pub async fn get_my_identity(
        &self,
    ) -> Result<crate::proto::chat_v1::GetMyIdentityResponse, AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client.get_my_identity().await.map_err(AuthError::Network)
    }

    /// Read the sealed-sender RECEIVING policy (allow_from_anyone) from the server.
    pub async fn get_sealed_sender_policy(&self) -> Result<bool, AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client
            .get_sealed_sender_policy()
            .await
            .map(|r| r.allow_from_anyone)
            .map_err(AuthError::Network)
    }

    /// Set the sealed-sender RECEIVING policy on the server. Returns the value
    /// the server persisted (echoed back).
    /// Point the server's UAK at `profile_key` after a rotation.
    pub async fn set_unidentified_access_key(
        &self,
        profile_key: &[u8; 32],
    ) -> Result<(), AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client
            .set_unidentified_access_key(&crate::crypto::derive_uak(profile_key))
            .await
            .map_err(AuthError::Network)
    }

    pub async fn set_sealed_sender_policy(
        &self,
        allow_from_anyone: bool,
    ) -> Result<bool, AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client
            .set_sealed_sender_policy(allow_from_anyone)
            .await
            .map(|r| r.allow_from_anyone)
            .map_err(AuthError::Network)
    }

    pub async fn login(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        device_info: &str,
        pin: Option<&str>,
        on_verifying: &(dyn Fn() + Send + Sync),
    ) -> Result<LoginResult, AuthError> {
        // Check not already logged in
        if self.is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn);
        }

        // PRIVACY: the server sees only the nickname-derived identifier, and
        // the password only through OPAQUE (`crypto::opaque`).
        let identifier = login_identifier(username);

        // Profile key: existing-device login loads it from local settings; a
        // fresh device has none here and recovers it from SVR backup AFTER
        // successful login (see `Client::attempt_svr_restore`). This avoids
        // the chicken-and-egg of needing profile_key to authenticate before
        // the authenticated SVR fetch can return profile_key.
        let stored_profile_key: Option<[u8; 32]> = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)
            .ok()
            .flatten()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok());

        // Derive SVR public key - required for registration
        //
        // PERF/CORRECTNESS: one Argon2id for both the keypair here and the PIN
        // fingerprint refreshed further down — same (pin, username), same
        // secret — and run off the async worker. See `derive_svr_secret_async`.
        let mut svr_secret: [u8; 32];
        let svr_public_key = if let Some(p) = pin {
            // Shape-only: this is the EXISTING account PIN being used to log in
            // and drive the post-login SVR restore, not a PIN being chosen.
            // Full strength validation here would lock a user out of their own
            // account for a PIN that was valid when set — the exact
            // ratchet-forward violation the validate_pin/validate_pin_shape
            // split exists to prevent (§10). A weak legacy PIN's cost is
            // already paid; refusing the login only denies the owner.
            crate::crypto::svr::validate_pin_shape(p)
                .map_err(|e| AuthError::LoginFailed(format!("invalid PIN: {e}")))?;
            let secret = crate::crypto::svr::derive_svr_secret_async(p, username).await?;
            let keypair = crate::crypto::svr::SvrKeypair::from_secret(&secret);
            svr_secret = secret;
            keypair.public_key_bytes().to_vec()
        } else {
            return Err(AuthError::LoginFailed(
                "PIN is required for account recovery".to_string(),
            ));
        };

        // Load the local identity key pair — and DO NOT create one if it is
        // absent.
        //
        // This used to mint a fresh identity here. That single line was the
        // root of the worst defect in the product: on a reinstall it produced
        // a local identity BEFORE the post-login SVR restore ran, so the
        // restore saw `has_local_identity() == true`, concluded this was an
        // existing device, logged "SVR restore skipped: local identity keys
        // already exist" and returned without ever fetching the PIN backup.
        // The user's real identity key was destroyed by the act of logging in
        // to recover it, and nothing said so.
        //
        // A missing identity is now carried out of here as a fact for the
        // caller to resolve (`ChatterClient::login` runs the SVR restore, and
        // only mints on an explicit user decision once recovery is ruled out).
        // Login itself needs no identity key: the request carries only a
        // device auth token.
        let identity_key_pair = self.identity_manager.get_identity()?.map(|(_, kp)| kp);

        // For login, device_id might come from settings (re-auth) or be assigned by server
        let stored_device_id: Option<u32> = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten();
        let device_id_for_creds = stored_device_id.unwrap_or(1);

        // Generate device credentials. Login never sends a UAK: the server's
        // copy is account-level and moves only at registration and via
        // SetUnidentifiedAccessKey, so a fresh device (no local profile_key
        // yet, so a throwaway key here) or one that missed a ProfileKeySync
        // cannot overwrite it.
        let credentials = match &stored_profile_key {
            Some(pk) => DeviceCredentials::generate(device_id_for_creds, pk),
            None => {
                let mut tmp_pk = generate_profile_key();
                let creds = DeviceCredentials::generate(device_id_for_creds, &tmp_pk);
                tmp_pk.zeroize();
                creds
            }
        };

        // OPAQUE login: KE1 -> KE2, then KE3 -> session. A wrong password
        // (or an unknown nickname, answered from a fake record) fails while
        // opening KE2, before anything else is sent. KE2 may first cost a
        // proof of work when the account has recent failed attempts.
        let opaque_start = opaque::login_start(password)?;
        let grant = {
            let mut g = self.recovery_login_grant.lock();
            match g.take() {
                Some((id, grant)) if id == identifier => Some(grant),
                other => {
                    *g = other;
                    None
                }
            }
        };
        // This device's trusted-device token for the account, if it has
        // logged in (or registered) here before. Unreadable = none: the login
        // then goes through the shared ceiling, which is what it would do
        // without a token anyway.
        let token_key = keys::trusted_device_token_key(&identifier);
        let stored_token = self.settings_repo.get_bytes(&token_key).ok().flatten();
        let mut device_token = stored_token.clone();
        let started = login_start_paying_pow(
            auth_client,
            &identifier,
            &opaque_start.ke1,
            grant.as_deref(),
            &mut device_token,
            on_verifying,
        )
        .await;
        if stored_token.is_some() && device_token.is_none() {
            // Revoked by the server (password changed, recovered, expired).
            let _ = self.settings_repo.delete(&token_key);
        }
        let started = started?;
        let opaque_done = opaque::login_finish_async(opaque_start, password, started.ke2).await?;
        let response = auth_client
            .login_finish(
                &started.login_id,
                &opaque_done.ke3,
                device_info,
                &svr_public_key, // Derived from PIN
                credentials.auth_token().as_bytes(),
                stored_device_id,
            )
            .await
            .map_err(|e| match &e {
                NetworkError::Unauthenticated => AuthError::InvalidCredentials,
                _ => AuthError::Network(e),
            })?;

        let identity_public_key = identity_key_pair
            .as_ref()
            .map(|kp| kp.identity_key().serialize());

        // The fresh trusted-device token for this device's next login. In
        // the encrypted settings database, like the device auth token.
        if !response.trusted_device_token.is_empty() {
            self.settings_repo
                .set_bytes(&token_key, &response.trusted_device_token)?;
        }

        // Extract session and user profile
        let session = response
            .session
            .ok_or_else(|| AuthError::LoginFailed("no session returned".to_string()))?;
        let user_profile = response
            .user_profile
            .ok_or_else(|| AuthError::LoginFailed("no user_profile returned".to_string()))?;

        let user_id: UserId = user_profile
            .user_id
            .parse()
            .map_err(|_| AuthError::LoginFailed("invalid user_id format".to_string()))?;
        let device_id = session.device_id;

        // LATENT BUG FIX (surfaced by validated.rs V51's fresh-install flow):
        // on a fresh device we generate credentials with a guessed device_id
        // (1), but the server binds the auth token to the device id it
        // actually ASSIGNS (e.g. 2 when the primary registration still holds
        // device 1). Basic auth is "{user_id}.{device_id}:{token}" — with the
        // stale id every authenticated RPC after login fails device auth.
        // Rebind the same token/UAK to the server-assigned id before
        // persisting or configuring the interceptor.
        let credentials = if credentials.device_id() != device_id {
            DeviceCredentials::from_stored(
                device_id,
                *credentials.auth_token().as_bytes(),
                *credentials.unidentified_access_key(),
            )
        } else {
            credentials
        };

        // PRIVACY: decrypt display_name with the locally-known profile_key.
        // Fresh-device case has no profile_key yet; the post-login SVR
        // restore plus a follow-up profile fetch will populate display_name.
        let display_name = match (&stored_profile_key, user_profile.display_name.is_empty()) {
            (_, true) => String::new(),
            // Login does not need the name, so an undecryptable one must not
            // fail it: keep whatever name this device already had.
            (Some(pk), false) => base64::engine::general_purpose::STANDARD
                .decode(&user_profile.display_name)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    crate::crypto::profile_crypto::decrypt_display_name(pk, &bytes)
                        .map_err(|e| e.to_string())
                })
                .unwrap_or_else(|e| {
                    tracing::warn!("login: display_name undecryptable, keeping local name: {e}");
                    self.settings_repo
                        .get_string(keys::DISPLAY_NAME)
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                }),
            (None, false) => String::new(),
        };

        // Persist auth state in settings
        self.settings_repo
            .set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &device_id)?;
        self.settings_repo
            .set_string(keys::DISPLAY_NAME, &display_name)?;
        // Salt for later PIN-derived flows (server message-backup from settings).
        self.settings_repo
            .set_string(keys::SVR_SALT_USERNAME, username)?;
        // Server-issued opaque push topic (empty when push is disabled).
        // configure_push subscribes to this instead of deriving it locally.
        self.settings_repo
            .set_string(keys::PUSH_TOPIC, &response.push_topic)?;

        // SECURITY: store auth token. profile_key is NOT touched on login -
        // it is set at registration (random) and on a fresh device is loaded
        // by SVR restore. Overwriting from login would either be a no-op or
        // corrupt state.
        self.settings_repo
            .set_bytes(keys::DEVICE_AUTH_TOKEN, credentials.auth_token().as_bytes())?;

        // Refresh local PIN fingerprint (re-auth gate for linked-device approval).
        // Reuses the SVR secret already derived above.
        if pin.is_some() {
            let mut fp = crate::crypto::svr::pin_fingerprint_from_secret(&svr_secret);
            self.settings_repo.set_bytes(keys::PIN_FINGERPRINT, &fp)?;
            fp.zeroize();
        }

        // SECURITY: done with the PIN-derived secret — drop it now.
        svr_secret.zeroize();

        // Store identity public key for NDSS'21 private friend codes.
        // This allows generating friend codes without accessing the SignalStore.
        // Absent on a reinstall until the SVR restore (or an explicit identity
        // reset) supplies one — writing a placeholder would hand out friend
        // codes for a key that does not exist.
        if let Some(pk) = identity_public_key.as_ref() {
            self.settings_repo
                .set_bytes(keys::IDENTITY_KEY_PUBLIC, pk.as_ref())?;
        }

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
            avatar_bytes: None,
            status_message: None,
            presence: PresenceStatus::Online,
        };
        self.user_repo.upsert_profile(&profile)?;

        Ok(LoginResult {
            user_id,
            device_id,
            display_name,
            needs_prekey_upload: stored_device_id != Some(device_id), // Server may replace a retired device
            keys_restored: false, // Set by caller after the SVR restore attempt
            // No local identity yet. The caller must resolve this before the
            // login can be called successful — see `ChatterClient::login`.
            key_recovery: identity_key_pair
                .is_none()
                .then_some(KeyRecoveryNeed::NoPinProvided),
            recovery_codes: None, // Login never returns recovery codes
        })
    }

    /// Mint a Signal identity key for a device that has none, after key
    /// recovery has been ruled out.
    ///
    /// Separate from `register`'s mint on purpose: this runs against an
    /// account that already exists on the server, so the new key must be
    /// PUBLISHED (see `SignalService::republish_identity_bundle`) or the
    /// account is left serving a key nobody holds — the exact corruption this
    /// change exists to eliminate. Callers must publish immediately after.
    ///
    /// `generate_identity` refuses to overwrite an existing identity, so this
    /// cannot silently destroy a working device.
    pub fn generate_identity_for_recovery(&self) -> Result<(), AuthError> {
        let (_, identity_key_pair) = self
            .identity_manager
            .generate_identity(&mut rand::rngs::OsRng.unwrap_err())?;

        // Friend-code generation reads this without touching the SignalStore;
        // leaving the previous device's key here would mint codes for a key
        // this device cannot prove it holds.
        self.settings_repo.set_bytes(
            keys::IDENTITY_KEY_PUBLIC,
            identity_key_pair.identity_key().serialize().as_ref(),
        )?;

        Ok(())
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
    ///
    /// # Arguments
    ///
    /// * `avatar` - Optional avatar image bytes (stored locally, encrypted when shared)
    pub async fn register(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        display_name: &str,
        device_info: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, AuthError> {
        // Check not already logged in
        if self.is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn);
        }

        // Recover from a prior aborted register: identity + prekeys may be
        // persisted locally without a user session (server call failed
        // after the crypto step). `generate_identity` refuses to overwrite,
        // so wipe the stale crypto state now. Safe because `is_logged_in`
        // is false — no real sessions to break.
        if self.identity_manager.has_identity()? {
            self.identity_manager.reset_for_new_registration()?;
        }

        // SECURITY: Profile key stays client-side only - never sent to server.
        // Signal-aligned: profile_key is 32 random bytes from CSPRNG, NOT
        // derived from password. See `crypto::auth_crypto::generate_profile_key`.
        // SVR backup uploads the wrapped profile_key after registration so
        // other devices can recover it via PIN.
        let profile_key = generate_profile_key();
        let username_hash = login_identifier(username);

        // Pick a Signal-style `nickname.discriminator` candidate. The random
        // discriminator binds into the Ristretto255 lookup hash so wordlist
        // enumeration of registered nicknames is infeasible. On uniqueness
        // collision the server rejects and the user re-runs registration.
        let full_username = username_candidates(username)
            .map_err(|e| AuthError::RegistrationFailed(format!("invalid nickname: {e}")))?
            .into_iter()
            .next()
            .ok_or_else(|| AuthError::RegistrationFailed("no username candidates".into()))?;

        // Derive SVR public key and registration lock - required for registration
        //
        // PERF: registration needs three PIN-derived values (SVR keypair,
        // registration lock, and the local PIN fingerprint set further down).
        // All three are HMAC/X25519 over the SAME Argon2id secret for the same
        // (pin, username), so the ~450ms/128 MiB KDF runs ONCE here and the
        // cheap derivations reuse it. It previously ran once per value —
        // identical inputs, bit-identical output, ~900ms of pure waste — and,
        // being called inline from async, it stalled the runtime's connection
        // task hard enough to kill the not-yet-issued challenge request. See
        // `derive_svr_secret_async` for that failure mode.
        let mut svr_secret: [u8; 32];
        let (svr_public_key, mut registration_lock) = if let Some(p) = pin {
            crate::crypto::svr::validate_pin(p)
                .map_err(|e| AuthError::RegistrationFailed(format!("invalid PIN: {e}")))?;
            let secret = crate::crypto::svr::derive_svr_secret_async(p, username).await?;
            let keypair = crate::crypto::svr::SvrKeypair::from_secret(&secret);
            // SECURITY: Registration lock prevents account takeover without PIN
            let reg_lock = crate::crypto::svr::registration_lock_from_secret(&secret);
            svr_secret = secret;
            (keypair.public_key_bytes().to_vec(), reg_lock)
        } else {
            return Err(AuthError::RegistrationFailed(
                "PIN is required for account recovery".to_string(),
            ));
        };

        // Generate new identity (always fresh for registration)
        let (registration_id, identity_key_pair) = self
            .identity_manager
            .generate_identity(&mut rand::rngs::OsRng.unwrap_err())?;

        // Generate signed prekey
        let signed_prekey = self.prekey_manager.generate_signed_prekey(
            &mut rand::rngs::OsRng.unwrap_err(),
            1,
            &identity_key_pair,
        )?;

        // Generate Kyber prekey for post-quantum resistance. This is the
        // medium-term LAST-RESORT PQSPK — it is reused across handshakes until
        // rotated, so it is a floor, not the primary PQ key material.
        let kyber_prekey = self
            .prekey_manager
            .generate_kyber_prekey(1, &identity_key_pair)?;

        // Generate one-time prekeys (100 for initial batch)
        let one_time_prekeys =
            self.prekey_manager
                .generate_prekeys(&mut rand::rngs::OsRng.unwrap_err(), 1, 100)?;

        // Generate the one-time Kyber prekey pool (PQOPK), same batch size as
        // the classical OPKs above. Per PQXDH §3.2 the server pops one PQOPK
        // for every classical OPK it hands out, so the two pools MUST be seeded
        // and refilled in lockstep — exactly as `SignalService::refresh_prekeys`
        // already does for the replenishment path.
        //
        // Seeding these at registration is what gives each session its OWN PQ
        // key. Without them the server has nothing to pop and every PQXDH
        // handshake falls back to the single last-resort PQSPK above, which is
        // spec-permitted but reuses one PQ key across every session until
        // rotation (see the warning at crypto/signal/identity.rs:38-42).
        //
        // IDs start at 2: the last-resort PQSPK holds id 1. `generate_kyber_prekey`
        // persists each record to the key store, so `get_next_kyber_prekey_id`
        // advances as we go — the same loop shape as the actor's
        // RefreshKyberPrekeys handler.
        let mut kyber_one_time_prekeys = Vec::with_capacity(100);
        for _ in 0..100 {
            let id = self.prekey_manager.get_next_kyber_prekey_id()?;
            kyber_one_time_prekeys.push(
                self.prekey_manager
                    .generate_kyber_prekey(id, &identity_key_pair)?,
            );
        }

        // Store profile key locally (needed for profile encryption)
        // Stored in SQLCipher encrypted database
        self.settings_repo
            .set_bytes(keys::PROFILE_KEY, &profile_key)?;
        // Persist the chosen nickname.discriminator so discovery / lookup
        // flows can reproduce the Ristretto255 hash without prompting.
        self.settings_repo
            .set_string(keys::FULL_USERNAME, &full_username)?;

        // Store local-only PIN fingerprint so re-auth gates (e.g. linked-device
        // approval) can verify the PIN without a server round-trip. Derived
        // from the same Argon2id SVR secret computed above (literally the same
        // bytes, not a second identical derivation); never sent to the server.
        if pin.is_some() {
            let mut fp = crate::crypto::svr::pin_fingerprint_from_secret(&svr_secret);
            self.settings_repo.set_bytes(keys::PIN_FINGERPRINT, &fp)?;
            fp.zeroize();
        }

        // SECURITY: the PIN-derived secret is done being used; drop it from
        // memory rather than letting it live to the end of registration.
        svr_secret.zeroize();

        // Store avatar locally if provided (raw bytes, encrypted when shared)
        // Avatar is NOT uploaded to server - only shared E2E encrypted during bootstraps
        if let Some(avatar_bytes) = avatar {
            self.settings_repo.set_bytes(keys::AVATAR, avatar_bytes)?;
        }

        // Derive UAK from profile key (16 bytes) - this is what we send to server
        let uak = crate::crypto::device_credentials::derive_uak(&profile_key);

        // SECURITY: Encrypt display_name with profile key before sending
        // Server never sees plaintext profile data - only the encrypted blob
        // Uses AES-256-GCM with padding to 128 bytes (Signal spec)
        let encrypted_display_name =
            crate::crypto::profile_crypto::encrypt_display_name(&profile_key, display_name)?;

        // Device ID is 1 for primary device during registration
        let device_id = 1u32;
        let credentials = DeviceCredentials::generate(device_id, &profile_key);

        // SECURITY: Request registration challenge from server
        // This prevents spoofing identity keys during registration
        let identity_public_key_full = identity_key_pair.identity_key().serialize();
        // Skip the first byte (type prefix) to get the raw 32-byte Curve25519 key
        let identity_public_key = &identity_public_key_full[1..];
        tracing::info!(
            "Requesting registration challenge - username_hash: {}, identity_key_len: {}",
            username_hash,
            identity_public_key.len()
        );
        // OPAQUE registration rides the challenge round trip: the server
        // answers our RegistrationRequest under a fresh credential identifier
        // bound to this challenge, and Register uploads the record.
        let opaque_reg = opaque::registration_start(password)?;
        let challenge = auth_client
            .request_registration_challenge(
                &username_hash,
                identity_public_key,
                &opaque_reg.request,
            )
            .await
            .map_err(|e| {
                tracing::error!("Challenge request failed: {}", e);
                AuthError::RegistrationFailed(format!("challenge request failed: {}", e))
            })?;
        let opaque_record = opaque::registration_finish_async(
            opaque_reg,
            password,
            challenge.opaque_registration_response.clone(),
        )
        .await?;

        // SECURITY: prove possession of the identity key we're uploading. The
        // signature is over a domain-separated message and a wrong-length nonce
        // is refused, so the server cannot use this as a prekey signing oracle
        // (see IdentityKeyManager::sign_registration_challenge).
        let challenge_response = self
            .identity_manager
            .sign_registration_challenge(&challenge.challenge_nonce)
            .map_err(AuthError::Crypto)?;

        // Proof of work the server set for this transport (required over the
        // onion service and rising with its load, 0 on clearnet), over the
        // challenge token. Solved on the blocking pool. Given
        // up after 4 min: the challenge lives 5 min on the server, and
        // measuring from our own clock (not `expires_at`) keeps a skewed
        // phone clock out of it.
        let pow_challenge = challenge.challenge.clone().ok_or_else(|| {
            AuthError::RegistrationFailed("challenge carries no proof-of-work token".to_string())
        })?;
        let pow_started = std::time::Instant::now();
        let pow_solution = crate::crypto::pow::solve_async(
            crate::crypto::pow::Purpose::Register,
            pow_challenge.token.clone(),
            pow_challenge.difficulty,
            pow_challenge.gate_bits,
            pow_started + std::time::Duration::from_secs(240),
        )
        .await
        .map_err(AuthError::Crypto)?;
        tracing::info!(
            pow_bits = pow_challenge.difficulty,
            gate_bits = pow_challenge.gate_bits,
            solve_ms = pow_started.elapsed().as_millis() as u64,
            "registration proof of work solved"
        );

        // Build Signal Protocol prekey bundle for server
        use crate::proto::chat_v1::SignalPreKeyBundle;
        use crate::proto::chat_v1::signal_pre_key_bundle::{
            KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord as ProtoPreKey,
            SignedPreKeyRecord as ProtoSignedPreKey,
        };
        use libsignal_protocol::GenericSignedPreKey;

        // Extract values from signed prekey (these are infallible for freshly generated keys)
        let signed_prekey_id: u32 = signed_prekey
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let signed_prekey_public = signed_prekey
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize();
        let signed_prekey_sig = signed_prekey
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        // Chatter-owned wire field is nanoseconds; libsignal-native Timestamp
        // stores milliseconds. Convert via ×1_000_000 on the wire.
        let signed_prekey_ts = signed_prekey
            .timestamp()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .epoch_millis()
            * 1_000_000;

        // Extract Kyber prekey values
        let kyber_id: u32 = kyber_prekey
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let kyber_public = kyber_prekey
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize();
        let kyber_sig = kyber_prekey
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        let prekey_bundle = SignalPreKeyBundle {
            identity_key: identity_public_key_full.to_vec(),
            registration_id,
            device_id,
            signed_prekey: Some(ProtoSignedPreKey {
                id: signed_prekey_id,
                public_key: signed_prekey_public.to_vec(),
                signature: signed_prekey_sig,
                timestamp: signed_prekey_ts,
            }),
            one_time_prekey: if let Some(pk) = one_time_prekeys.first() {
                let pk_id: u32 = pk
                    .id()
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?
                    .into();
                let pk_public = pk
                    .public_key()
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?
                    .serialize();
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
                    .as_nanos() as u64,
            }),
            // Per NDSS '21: Do NOT enable unrestricted access by default.
            // Initial contact uses anonymous credentials (blind-signed tokens) instead.
            // This prevents spam while preserving sender anonymity.
            unrestricted_unidentified_access: Some(false),
        };

        // Build initial one-time prekeys for server (skip first, it's in bundle)
        let mut initial_prekeys: Vec<ProtoPreKey> = Vec::new();
        for pk in one_time_prekeys.iter().skip(1) {
            let pk_id: u32 = pk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let pk_public = pk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize();
            initial_prekeys.push(ProtoPreKey {
                id: pk_id,
                public_key: pk_public.to_vec(),
            });
        }

        // Build the PQOPK batch for the server. Unlike the classical OPK, every
        // PQ prekey carries an XEdDSA signature over its public key (PQXDH §3.2);
        // the server verifies it via ValidatePreKeySignatures and rejects the
        // batch outright if it does not check out. None are held back for the
        // bundle: the bundle's `kyber_prekey` field is the last-resort PQSPK, and
        // the server picks a PQOPK from this pool at handout time.
        let mut initial_kyber_prekeys: Vec<ProtoKyberPreKey> = Vec::new();
        for kpk in &kyber_one_time_prekeys {
            let id: u32 = kpk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let public = kpk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize();
            let sig = kpk
                .signature()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?;
            // Chatter-owned wire field is nanoseconds; libsignal stores millis.
            let ts = kpk
                .timestamp()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .epoch_millis()
                * 1_000_000;
            initial_kyber_prekeys.push(ProtoKyberPreKey {
                id,
                public_key: public.to_vec(),
                signature: sig,
                timestamp: ts,
            });
        }

        // Call server registration (wait for completion)
        // PRIVACY: Send username_hash, NOT plaintext username
        // SECURITY: Send UAK (16 bytes), NOT profile_key (32 bytes)
        // Profile key stays client-side for profile encryption
        // SECURITY: display_name is encrypted with profile key (server sees only ciphertext)
        // SECURITY: the challenge token and response prove identity key possession
        //
        // Signal-style discovery hash: Ristretto255 commitment over the
        // already-chosen `nickname.discriminator` (see `full_username` above),
        // plus the zero-knowledge proof the server verifies it against.
        let (username_lookup, username_proof) = username_claim(&full_username)
            .map_err(|e| AuthError::RegistrationFailed(format!("lookup hash failed: {e}")))?;
        let result = auth_client
            .register_and_wait(
                &username_hash, // PRIVACY: Send hash, not plaintext username
                &opaque_record, // OPAQUE RegistrationRecord: nothing password-equivalent
                &encrypted_display_name,
                &svr_public_key, // Derived from PIN
                device_info,
                credentials.auth_token().as_bytes(),
                &uak,
                Some(prekey_bundle),
                initial_prekeys,
                &pow_challenge.token,
                &challenge_response,
                pow_solution,
                &registration_lock, // SECURITY: Prevents account takeover without PIN
                &username_lookup,
                &username_proof,
            )
            .await;

        // SECURITY: Zeroize registration lock immediately after use (both success and error paths)
        registration_lock.zeroize();

        let result = result.map_err(|e| AuthError::RegistrationFailed(e.to_string()))?;

        // Extract user_id from registration result (server-assigned)
        let user_id: UserId = result
            .user_id
            .parse()
            .map_err(|_| AuthError::RegistrationFailed("invalid user_id format".to_string()))?;

        // Persist auth state (device_id is server-assigned)
        self.settings_repo
            .set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &result.device_id)?;
        self.settings_repo
            .set_string(keys::DISPLAY_NAME, display_name)?;
        // Salt for later PIN-derived flows (server message-backup from settings).
        self.settings_repo
            .set_string(keys::SVR_SALT_USERNAME, username)?;

        // SECURITY: Store auth token in encrypted settings
        self.settings_repo
            .set_bytes(keys::DEVICE_AUTH_TOKEN, credentials.auth_token().as_bytes())?;
        // This device created the password: it is trusted for its next login
        // (server OPEN-ITEMS §134). Encrypted settings, like the auth token.
        if !result.trusted_device_token.is_empty() {
            self.settings_repo.set_bytes(
                &keys::trusted_device_token_key(&username_hash),
                &result.trusted_device_token,
            )?;
        }

        // Store identity public key for NDSS'21 private friend codes
        // This allows generating friend codes without accessing the SignalStore
        // Store the full serialized key (33 bytes) as it's used for Signal Protocol
        self.settings_repo
            .set_bytes(keys::IDENTITY_KEY_PUBLIC, identity_public_key_full.as_ref())?;

        // Configure auth interceptor for Basic auth
        // Client already has device_auth_token and UAK (generated locally)
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        // Seed the server's PQOPK pool. This reuses the existing RefreshPreKeys
        // RPC rather than adding a registration field, which also fixes the
        // ordering constraints for free:
        //   - signal_kyber_one_time_prekeys has an FK on
        //     signal_prekey_bundles(user_id, device_id), so the bundle must
        //     already be stored — i.e. after register_and_wait.
        //   - RefreshPreKeys is authenticated, so this must follow set_basic.
        // The classical OPKs ride along inside RegisterRequest and are already
        // uploaded by this point; only the PQ pool is sent here.
        //
        // A failure here is logged, not fatal: the account is already registered
        // server-side, and PQXDH still succeeds against the last-resort PQSPK
        // (spec-permitted). Failing registration outright would be a worse
        // outcome than the degradation it avoids. The background replenisher
        // (app/core_services.rs) refills both pools together on its next pass.
        if !initial_kyber_prekeys.is_empty() {
            let pq_count = initial_kyber_prekeys.len();
            match auth_client
                .refresh_pre_keys_with_kyber_otpk(Vec::new(), initial_kyber_prekeys)
                .await
            {
                Ok(()) => {
                    tracing::info!(pq_opk = pq_count, "Seeded PQOPK pool at registration");
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "Failed to seed PQOPK pool at registration — PQXDH will fall back to \
                         the last-resort PQSPK until the replenisher runs"
                    );
                }
            }
        }

        // Store user locally
        let user = User {
            id: user_id,
            username: username.to_string(),
            svr_public_key: Vec::new(), // SVR key will be set during PIN setup
            last_active: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
        };
        self.user_repo.insert(&user)?;

        // PRIVACY: For registration, use the provided plaintext display_name.
        // Server returns encrypted version which we don't need to decrypt since we have the original.
        let server_display_name = display_name.to_string();

        let profile = Profile {
            user_id,
            display_name: server_display_name.clone(),
            avatar_bytes: None,
            status_message: None,
            presence: PresenceStatus::Online,
        };
        self.user_repo.upsert_profile(&profile)?;

        // Capture recovery codes from server (shown once to user, never stored)
        let recovery_codes = if result.recovery_codes.is_empty() {
            None
        } else {
            Some(result.recovery_codes.clone())
        };

        Ok(LoginResult {
            user_id,
            device_id: result.device_id,
            display_name: server_display_name,
            needs_prekey_upload: false, // Prekeys already generated (upload them next)
            keys_restored: false, // Not applicable for registration (keys are freshly generated)
            key_recovery: None,
            recovery_codes,
        })
    }

    /// End authorization without retiring this device or touching its local data.
    pub(crate) fn end_session(&self) -> Result<(), AuthError> {
        self.auth_interceptor.clear();
        self.settings_repo.delete(keys::DEVICE_AUTH_TOKEN)?;
        // PIN authorization must be re-derived from user input as well.
        self.settings_repo.delete(keys::PIN_FINGERPRINT)?;
        Ok(())
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

        // Clear auth interceptor state
        self.auth_interceptor.clear();

        // Clear auth state from settings (but keep identity keys)
        self.settings_repo.delete(keys::CURRENT_USER_ID)?;
        self.settings_repo.delete(keys::CURRENT_DEVICE_ID)?;
        self.settings_repo.delete(keys::DEVICE_AUTH_TOKEN)?;

        // Clear the local PIN fingerprint — re-derived on next login from
        // user-supplied PIN. Leaving stale fingerprints behind would let a
        // linked-device approve flow run after logout if the modal stayed
        // open in another window.
        let _ = self.settings_repo.delete(keys::PIN_FINGERPRINT);

        Ok(())
    }

    // ==================== Simplified Wrappers ====================

    /// Simplified wrapper for login that creates the client internally.
    pub async fn do_login(
        &self,
        username: &str,
        password: &str,
        device_name: &str,
        pin: Option<&str>,
        on_verifying: &(dyn Fn() + Send + Sync),
    ) -> Result<LoginResult, AuthError> {
        let mut auth_client = self.try_auth_client().ok_or_else(|| {
            AuthError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".to_string(),
            ))
        })?;
        self.login(
            &mut auth_client,
            username,
            password,
            device_name,
            pin,
            on_verifying,
        )
        .await
    }

    /// Simplified wrapper for register that creates the client internally.
    ///
    /// # Arguments
    ///
    /// * `avatar` - Optional avatar image bytes (stored locally, encrypted when shared)
    pub async fn do_register(
        &self,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, AuthError> {
        let mut auth_client = self
            .try_auth_client()
            .ok_or_else(|| AuthError::RegistrationFailed("Not connected to server".to_string()))?;
        self.register(
            &mut auth_client,
            username,
            password,
            display_name,
            device_name,
            avatar,
            pin,
        )
        .await
    }

    /// Simplified wrapper for logout that creates the client internally.
    pub async fn do_logout(&self) -> Result<(), AuthError> {
        let mut auth_client = self.try_auth_client().ok_or_else(|| {
            AuthError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".to_string(),
            ))
        })?;
        self.logout(&mut auth_client).await
    }

    /// Change the user's password.
    ///
    /// One OPAQUE exchange proving the current password and one OPAQUE
    /// registration for the new one, in two round trips. The login identifier
    /// does not change (it is nickname-derived). The server revokes every
    /// other device session on success.
    pub async fn change_password(
        &self,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        if !self.is_logged_in() {
            return Err(AuthError::NotLoggedIn);
        }
        let mut auth_client = self.try_auth_client().ok_or_else(|| {
            AuthError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".to_string(),
            ))
        })?;
        change_password_with(&mut auth_client, current_password, new_password).await
    }

    /// Clean up partial state after a failed registration attempt.
    ///
    /// This removes any locally stored data that was created before the
    /// server registration failed (e.g., profile key, identity keys, prekeys).
    pub async fn cleanup_failed_registration(&self) -> Result<(), AuthError> {
        tracing::debug!("Cleaning up after failed registration");

        // Clear all crypto state (identity keys, prekeys, sessions)
        // This is the most important cleanup - prevents "UNIQUE constraint failed"
        if let Err(e) = self.key_store.clear_all_crypto_state() {
            tracing::warn!("Failed to clear crypto state: {}", e);
        }

        // Remove profile key if it was stored
        let _ = self.settings_repo.delete(keys::PROFILE_KEY);

        // Remove any partial auth state
        let _ = self.settings_repo.delete(keys::CURRENT_USER_ID);
        let _ = self.settings_repo.delete(keys::CURRENT_DEVICE_ID);
        let _ = self.settings_repo.delete(keys::DEVICE_AUTH_TOKEN);

        // Remove sender certificate if any
        let _ = self.settings_repo.delete(keys::SENDER_CERTIFICATE);
        let _ = self
            .settings_repo
            .delete(keys::SENDER_CERTIFICATE_REFRESH_AFTER);
        let _ = self
            .settings_repo
            .delete(keys::SENDER_CERTIFICATE_SERVER_KEY_ID);

        // Clear auth interceptor
        self.auth_interceptor.clear();

        Ok(())
    }

    /// Redeem a recovery code to reset password + PIN.
    ///
    /// Validates inputs, derives the new SVR material from `(new_pin,
    /// username)` (`username` is the bare nickname, still needed to salt it),
    /// and runs the two-step redemption: the code plus a fresh OPAQUE
    /// registration for `new_password`. Returns the number of recovery codes
    /// that remain on the account. All sensitive intermediates are zeroized
    /// before return.
    pub async fn use_recovery_code(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        recovery_code: &str,
        new_password: &str,
        new_pin: &str,
    ) -> Result<i32, AuthError> {
        use zeroize::Zeroize;

        crate::crypto::svr::validate_pin(new_pin)
            .map_err(|e| AuthError::InvalidInput(e.to_string()))?;
        let recovery_code = normalize_recovery_code(recovery_code).ok_or_else(|| {
            AuthError::InvalidInput(format!(
                "Recovery code must be {RECOVERY_CODE_LEN} characters (letters and digits 2-9)"
            ))
        })?;

        let keypair = crate::crypto::svr::SvrKeypair::new_from_pin(new_pin, username)?;
        let svr_public_key = keypair.public_key_bytes();

        let mut registration_lock =
            crate::crypto::svr::derive_registration_lock(new_pin, username)?;

        // The server finds the account by the code itself. The new password is
        // a fresh OPAQUE registration; the login identifier stays as it was.
        let opaque_reg = opaque::registration_start(new_password)?;
        let result = async {
            let started = auth_client
                .use_recovery_code_start(&recovery_code, &opaque_reg.request)
                .await?;
            let record = opaque::registration_finish_async(
                opaque_reg,
                new_password,
                started.registration_response,
            )
            .await?;
            auth_client
                .use_recovery_code_finish(
                    &started.recovery_id,
                    &record,
                    &svr_public_key,
                    &registration_lock,
                )
                .await
                .map_err(AuthError::Network)
        }
        .await;

        registration_lock.zeroize();

        // The grant lets the login with the new password through even while
        // someone keeps the account's login guess ceiling full.
        let finished = result?;
        if !finished.login_grant.is_empty() {
            *self.recovery_login_grant.lock() =
                Some((login_identifier(username), finished.login_grant));
        }
        Ok(finished.remaining_codes)
    }

    /// Regenerate the caller's recovery codes.
    ///
    /// Proves the password (OPAQUE, [`prove_password`]) and spends the proof
    /// on the server RPC. Returns the newly issued codes (caller must display
    /// them exactly once — codes are never recoverable from the server).
    pub async fn regenerate_recovery_codes(
        &self,
        auth_client: &mut AuthClient,
        password: &str,
    ) -> Result<Vec<String>, AuthError> {
        let proof = prove_password(auth_client, password).await?;
        auth_client
            .regenerate_recovery_codes(proof)
            .await
            .map_err(AuthError::Network)
    }
}

/// Password change against `auth_client` (authenticated): KE1 for the current
/// password plus the new password's RegistrationRequest, then KE3 plus the new
/// RegistrationRecord. A wrong current password fails client-side, before the
/// second round trip, with [`AuthError::InvalidCredentials`].
pub(crate) async fn change_password_with(
    auth_client: &mut AuthClient,
    current_password: &str,
    new_password: &str,
) -> Result<(), AuthError> {
    let proof = opaque::login_start(current_password)?;
    let reg = opaque::registration_start(new_password)?;
    let started = auth_client
        .change_password_start(&proof.ke1, &reg.request)
        .await?;
    let done = opaque::login_finish_async(proof, current_password, started.ke2).await?;
    let record =
        opaque::registration_finish_async(reg, new_password, started.registration_response).await?;
    auth_client
        .change_password_finish(&started.flow_id, &done.ke3, &record)
        .await
        .map_err(|e| match &e {
            NetworkError::Unauthenticated => AuthError::InvalidCredentials,
            _ => AuthError::Network(e),
        })
}

/// Length of a recovery code (server `recoveryCodeLength`): 26 symbols from a
/// 31-symbol alphabet, ~128.8 bits. The code alone identifies the account at
/// recovery, so it has to be a full-strength secret.
pub const RECOVERY_CODE_LEN: usize = 26;
const RECOVERY_CODE_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Reduce a typed recovery code to the canonical form the server stores a
/// digest of: separators (`-`, spaces) dropped, letters uppercased. `None`
/// when the result is not exactly [`RECOVERY_CODE_LEN`] alphabet symbols.
pub fn normalize_recovery_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == RECOVERY_CODE_LEN && code.chars().all(|c| RECOVERY_CODE_ALPHABET.contains(c)))
        .then_some(code)
}

#[cfg(test)]
mod recovery_code_tests {
    use super::*;

    #[test]
    fn normalize_accepts_grouped_lowercase_and_spaced_input() {
        let canonical = "ABCDEFGHJKMNPQRSTUVWXYZ234";
        assert_eq!(
            normalize_recovery_code(canonical).as_deref(),
            Some(canonical)
        );
        assert_eq!(
            normalize_recovery_code(" abcde-fghjk-mnpqr-stuvw-xyz234 ").as_deref(),
            Some(canonical)
        );
        assert_eq!(
            normalize_recovery_code("ABCDE FGHJK MNPQR STUVW XYZ234").as_deref(),
            Some(canonical)
        );
    }

    #[test]
    fn normalize_rejects_wrong_length_and_ambiguous_symbols() {
        // The old 12-character format is no longer a code.
        assert!(normalize_recovery_code("ABCDEFGHJKMN").is_none());
        assert!(normalize_recovery_code("ABCDEFGHJKMNPQRSTUVWXYZ2345").is_none());
        // 0/O/1/I/L are excluded from the alphabet.
        assert!(normalize_recovery_code("0BCDEFGHJKMNPQRSTUVWXYZ234").is_none());
        assert!(normalize_recovery_code("IBCDEFGHJKMNPQRSTUVWXYZ234").is_none());
        assert!(normalize_recovery_code("").is_none());
    }
}
