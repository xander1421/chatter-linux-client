//! Device linking service.
//!
//! Phase 2 of the multi-device work: orchestrates the QR-code-based device
//! linking flow on **both** sides of the protocol.
//!
//! - **New device** (unauthenticated): generates an ephemeral X25519
//!   provisioning keypair, calls `start_provisioning`, surfaces the QR URL
//!   to the UI, polls until the primary uploads an encrypted envelope,
//!   decrypts it, then completes linking with its own prekey bundle.
//! - **Primary device** (authenticated): parses a pasted/scanned QR URL,
//!   builds a `ProvisionMessage` from local identity + profile state,
//!   encrypts it for the new device's ephemeral pub, and uploads via
//!   `provision_device`.
//!
//! All envelope crypto goes through [`crate::crypto::provisioning`] which is
//! byte-format-aligned with Signal's `ProvisioningCipher` (see
//! `project_provisioning_byte_format.md` memory).

use std::sync::Arc;
use std::time::Duration;

use libsignal_protocol::{IdentityKey, IdentityKeyPair, PrivateKey};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use url::Url;
use zeroize::Zeroizing;

use crate::crypto::auth_crypto::{derive_profile_key, hash_password};
use crate::crypto::provisioning::{
    decrypt_envelope, encrypt_envelope, ProvisioningKeypair,
};
use crate::crypto::signal::identity::IdentityKeyManager;
use crate::crypto::signal::prekey::PreKeyManager;
use crate::crypto::DeviceCredentials;
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::domain::notification::NotificationPriority;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::services::notification_service::NotificationService;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::provisioning_client::{ProvisioningClient, ProvisioningPollResult};
use crate::proto::provisioning_v1::{ProvisionEnvelope, ProvisionMessage, ProvisioningSession};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::storage::repositories::user_repo::UserRepository;
use crate::storage::KeyStore;

/// URL scheme prefix for QR payloads.
pub const LINK_URL_SCHEME: &str = "chatter";
const LINK_URL_HOST: &str = "link";

/// Errors from device linking flows.
#[derive(Debug, thiserror::Error)]
pub enum LinkDeviceError {
    #[error("not logged in")]
    NotLoggedIn,
    #[error("re-authentication failed")]
    ReauthFailed,
    #[error("re-auth not supported for OIDC sessions")]
    ReauthUnsupportedForOidc,
    #[error("link URL malformed: {0}")]
    InvalidLinkUrl(String),
    #[error("provisioning session expired or cancelled: {0}")]
    SessionEnded(String),
    #[error("network: {0}")]
    Network(#[from] NetworkError),
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("invalid input: {0}")]
    Invalid(String),
}

/// Persistent state for a new-device provisioning session.
///
/// Owns the ephemeral keypair until the envelope arrives — losing this
/// (e.g. process restart) invalidates the in-flight session.
pub struct NewDeviceSession {
    /// Server-issued session UUID (also embedded in QR URL).
    pub uuid: String,
    /// `chatter://link?…` URL for QR encoding.
    pub qr_url: String,
    /// Server-side session expiry (Unix nanoseconds).
    pub expires_at: u64,
    /// Owned ephemeral keypair; secret zeroizes on drop.
    pub(crate) ephemeral: ProvisioningKeypair,
    pub device_info: String,
    pub device_name: String,
}

impl NewDeviceSession {
    pub fn public_key(&self) -> [u8; 32] {
        self.ephemeral.public_bytes()
    }
}

/// Pre-image of a parsed link URL (primary side input).
#[derive(Debug, Clone)]
pub struct LinkRequest {
    pub uuid: String,
    /// Raw 32-byte X25519 ephemeral public key from new device.
    pub peer_pub: [u8; 32],
    /// Server URL hint embedded by the new device (so primary can warn on
    /// cross-server linking attempts).
    pub server_url: Option<String>,
    pub device_name: Option<String>,
}

/// Result of completing linking on the new device side.
#[derive(Debug, Clone)]
pub struct CompletedLink {
    pub user_id: crate::domain::ids::UserId,
    pub device_id: u32,
    pub display_name: Option<String>,
}

/// Streamed events from the new-device flow. Consumed by UI as a single
/// stream — the UI never has to interleave start/poll/complete itself.
#[derive(Debug, Clone)]
pub enum LinkProgress {
    /// QR rendered + ready to scan/paste. Includes the link URL and a PNG
    /// of the QR code (already-rendered to avoid the UI depending on the
    /// `qrcode` crate).
    QrReady {
        url: String,
        qr_png: Vec<u8>,
        expires_at: u64,
    },
    /// Polling started — primary hasn't approved yet.
    WaitingForApproval,
    /// Primary approved — keys received, finishing linking.
    Approved,
    /// Linking complete — caller should switch to main view + run post-auth
    /// bring-up.
    Linked(CompletedLink),
    /// Flow ended unsuccessfully. `String` is a user-facing message.
    Failed(String),
    /// Cancelled by user or session expiry.
    Cancelled,
}

/// Device linking service.
pub struct LinkDeviceService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    user_repo: Arc<UserRepository>,
    key_store: Arc<KeyStore>,
    event_bus: EventBus,
    notifications: Arc<NotificationService>,
    /// UUID of an in-flight new-device session, owned here so the cancel
    /// button can reach it without the UI tracking session state.
    active_uuid: Mutex<Option<String>>,
}

impl LinkDeviceService {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        user_repo: Arc<UserRepository>,
        key_store: Arc<KeyStore>,
        event_bus: EventBus,
        notifications: Arc<NotificationService>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            user_repo,
            key_store,
            event_bus,
            notifications,
            active_uuid: Mutex::new(None),
        }
    }

    /// Platform/model string for `device_info` field. Single source of
    /// truth — both new-device and approve flows should use this.
    pub fn current_device_info() -> &'static str {
        if cfg!(target_os = "linux") {
            "Linux Desktop"
        } else if cfg!(target_os = "windows") {
            "Windows Desktop"
        } else if cfg!(target_os = "macos") {
            "macOS Desktop"
        } else if cfg!(target_os = "android") {
            "Android"
        } else if cfg!(target_os = "ios") {
            "iOS"
        } else {
            "Chatter Client"
        }
    }

    /// True if a new-device flow is currently running.
    pub fn has_active_session(&self) -> bool {
        self.active_uuid.lock().is_some()
    }

    /// Cancel the in-flight new-device session if any. No-op if no session.
    /// The poll loop notices the server-side cancel on its next tick and
    /// emits [`LinkProgress::Cancelled`].
    pub async fn cancel_active(&self) -> Result<(), LinkDeviceError> {
        let uuid = self.active_uuid.lock().take();
        if let Some(uuid) = uuid {
            let mut client = self.provisioning_client()?;
            client.cancel_provisioning(&uuid).await?;
        }
        Ok(())
    }

    /// Run the full new-device flow as a stream of progress events.
    ///
    /// Spawns a background task that owns the [`NewDeviceSession`] (and its
    /// ephemeral keypair) end-to-end. The returned receiver yields one or
    /// more [`LinkProgress`] events and then closes. The terminal event is
    /// always [`LinkProgress::Linked`], [`Failed`], or [`Cancelled`].
    ///
    /// The UI binding's only responsibilities are: render `QrReady`, update
    /// status text on `WaitingForApproval` / `Approved`, and switch view on
    /// the terminal event. No session state, no error string formatting,
    /// no polling.
    pub fn run_new_device_flow(
        self: &Arc<Self>,
        device_info: String,
        device_name: String,
    ) -> mpsc::UnboundedReceiver<LinkProgress> {
        let (tx, rx) = mpsc::unbounded_channel();
        let svc = self.clone();
        tokio::spawn(async move {
            run_flow(svc, device_info, device_name, tx).await;
        });
        rx
    }

    /// Re-auth gate using the user's PIN. Validates against the local
    /// `PIN_FINGERPRINT` setting (HMAC-derived from the SVR Argon2id
    /// secret). Local-only check — does not contact the server.
    /// Constant-time comparison.
    ///
    /// Returns `Err(Invalid)` if no PIN fingerprint is stored locally
    /// (legacy account that registered before PIN persistence landed —
    /// caller should prompt for re-login to populate it).
    pub fn verify_pin(&self, pin: &str) -> Result<bool, LinkDeviceError> {
        use subtle::ConstantTimeEq;

        if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
            return Err(LinkDeviceError::Invalid("PIN must be 6 digits".into()));
        }

        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(LinkDeviceError::NotLoggedIn)?;
        let user_id: crate::domain::ids::UserId = user_id_str
            .parse()
            .map_err(|e| LinkDeviceError::Invalid(format!("stored user_id parse: {e}")))?;
        let user = self
            .user_repo
            .get(&user_id)?
            .ok_or(LinkDeviceError::NotLoggedIn)?;

        let stored = self
            .settings_repo
            .get_bytes(keys::PIN_FINGERPRINT)?
            .ok_or_else(|| {
                LinkDeviceError::Invalid(
                    "no local PIN fingerprint — sign out and back in to populate".into(),
                )
            })?;
        if stored.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "stored PIN fingerprint has wrong length".into(),
            ));
        }

        let derived = crate::crypto::svr::derive_pin_fingerprint(pin, &user.username)?;
        Ok(derived.ct_eq(stored.as_slice()).unwrap_u8() == 1)
    }

    /// Convenience: verify EITHER a password OR a 6-digit PIN against local
    /// material. Tries the cheaper password check first when input doesn't
    /// look like a PIN; otherwise falls through to PIN. Lets the UI keep a
    /// single "confirm with password or PIN" input regardless of whether
    /// the session is password- or OIDC-based.
    pub fn verify_password_or_pin(&self, secret: &str) -> Result<bool, LinkDeviceError> {
        let looks_like_pin = secret.len() == 6 && secret.chars().all(|c| c.is_ascii_digit());
        if looks_like_pin {
            return self.verify_pin(secret);
        }
        match self.verify_password(secret) {
            Ok(v) => Ok(v),
            Err(LinkDeviceError::ReauthUnsupportedForOidc) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Re-auth gate. Validates the user's password against locally-stored
    /// `profile_key` material. **Local-only check** — does not contact the
    /// server. Constant-time comparison.
    ///
    /// Returns `Err(ReauthUnsupportedForOidc)` if no profile key is stored
    /// (i.e. user is on an OIDC session). Caller should fall back to
    /// [`Self::verify_pin`].
    pub fn verify_password(&self, password: &str) -> Result<bool, LinkDeviceError> {
        use subtle::ConstantTimeEq;

        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(LinkDeviceError::NotLoggedIn)?;
        let user_id: crate::domain::ids::UserId = user_id_str
            .parse()
            .map_err(|e| LinkDeviceError::Invalid(format!("stored user_id parse: {e}")))?;

        let user = self
            .user_repo
            .get(&user_id)?
            .ok_or(LinkDeviceError::NotLoggedIn)?;

        let stored_profile_key = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .ok_or(LinkDeviceError::ReauthUnsupportedForOidc)?;
        if stored_profile_key.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "stored profile key has wrong length".into(),
            ));
        }

        let password_hash = hash_password(password);
        let derived = derive_profile_key(&password_hash, &user.username)?;

        Ok(derived.ct_eq(stored_profile_key.as_slice()).unwrap_u8() == 1)
    }

    fn provisioning_client(&self) -> Result<ProvisioningClient, LinkDeviceError> {
        self.grpc_pool
            .try_channel()
            .map(|ch| ProvisioningClient::new(ch, self.grpc_pool.auth_clone()))
            .ok_or(LinkDeviceError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".into(),
            )))
    }

    // ====================================================================
    // New-device side (unauthenticated)
    // ====================================================================

    /// Start a provisioning session as a new device.
    ///
    /// Generates the ephemeral X25519 keypair locally, calls the server,
    /// builds the QR URL. The returned session must be kept in memory until
    /// the envelope arrives — its private key cannot be persisted safely
    /// without compromising secrecy.
    pub async fn start_as_new_device(
        &self,
        device_info: &str,
        device_name: &str,
    ) -> Result<NewDeviceSession, LinkDeviceError> {
        let ephemeral = ProvisioningKeypair::generate();
        let pub_b64 = base64_url(&ephemeral.public_bytes());

        let mut client = self.provisioning_client()?;
        let response = client
            .start_provisioning(device_info, device_name)
            .await?;

        let server_url = self.grpc_pool.endpoint();
        let qr_url = build_link_url(
            &response.provisioning_uuid,
            &pub_b64,
            Some(&server_url),
            Some(device_name),
        );

        Ok(NewDeviceSession {
            uuid: response.provisioning_uuid,
            qr_url,
            expires_at: response.expires_at,
            ephemeral,
            device_info: device_info.to_string(),
            device_name: device_name.to_string(),
        })
    }

    /// Poll until the primary uploads the envelope.
    ///
    /// Returns the decrypted [`ProvisionMessage`] on success. Caller is then
    /// responsible for [`complete_linking`] to finalize the device on the
    /// server.
    pub async fn poll_until_envelope(
        &self,
        session: &NewDeviceSession,
    ) -> Result<ProvisionMessage, LinkDeviceError> {
        let mut client = self.provisioning_client()?;

        loop {
            let result = client.poll_provisioning_message(&session.uuid).await?;
            match result {
                ProvisioningPollResult::Envelope(env) => {
                    return decrypt_envelope(&session.ephemeral, &env)
                        .map_err(LinkDeviceError::Crypto);
                }
                ProvisioningPollResult::Pending { retry_after_ms } => {
                    let delay = retry_after_ms.clamp(1_000, 10_000);
                    tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                }
                ProvisioningPollResult::Expired { message }
                | ProvisioningPollResult::Cancelled { message } => {
                    return Err(LinkDeviceError::SessionEnded(message));
                }
            }
        }
    }

    /// Complete the link using the decrypted [`ProvisionMessage`] and an
    /// already-generated prekey bundle.
    ///
    /// On success this:
    /// 1. Imports the primary's identity keypair into the local key store.
    /// 2. Persists current_user_id, device_id, profile key, auth token.
    /// 3. Configures the auth interceptor for Basic auth.
    ///
    /// The caller is responsible for: generating the prekey bundle (use
    /// [`PreKeyManager`] against a fresh registration ID), kicking off
    /// background services, and routing the UI to the main view.
    pub async fn complete_linking(
        &self,
        session: NewDeviceSession,
        provision: ProvisionMessage,
    ) -> Result<CompletedLink, LinkDeviceError> {
        if provision.aci_identity_key_private.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.aci_identity_key_private must be 32 bytes".into(),
            ));
        }
        if provision.profile_key.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.profile_key must be 32 bytes".into(),
            ));
        }
        if provision.aci.is_empty() {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.aci is required".into(),
            ));
        }

        let user_id: crate::domain::ids::UserId = provision
            .aci
            .parse()
            .map_err(|e| LinkDeviceError::Invalid(format!("invalid aci: {e}")))?;

        // Reconstruct identity keypair from primary's private key.
        let priv_bytes = Zeroizing::new(provision.aci_identity_key_private.clone());
        let private = PrivateKey::deserialize(priv_bytes.as_slice())
            .map_err(|e| CryptoError::Protocol(format!("identity priv parse: {e}")))?;
        let public = private
            .public_key()
            .map_err(|e| CryptoError::Protocol(format!("derive identity pub: {e}")))?;
        let identity_key = IdentityKey::new(public);
        let identity_pair = IdentityKeyPair::new(identity_key, private);

        // Generate fresh registration ID + keys synchronously and drop the
        // (`!Send`) ThreadRng before any `.await`, so the surrounding async
        // fn stays Send for tokio::spawn.
        //
        // Moved into a non-async inner fn: Rust's async drop tracking is
        // conservative around `?` early-returns — keeping `rand::rng()` in a
        // plain fn guarantees the `ThreadRng` local never ends up in the async
        // state machine.
        let prekey_manager = PreKeyManager::new(self.key_store.clone());
        #[allow(clippy::type_complexity)]
        fn gen_keys(
            prekey_manager: &PreKeyManager,
            identity_pair: &IdentityKeyPair,
        ) -> Result<
            (
                u32,
                libsignal_protocol::SignedPreKeyRecord,
                libsignal_protocol::KyberPreKeyRecord,
                Vec<libsignal_protocol::PreKeyRecord>,
            ),
            CryptoError,
        > {
            let mut rng = rand::rng();
            let reg_id = (rand::Rng::random::<u32>(&mut rng) % 0x3FFF) + 1;
            let signed =
                prekey_manager.generate_signed_prekey(&mut rng, 1, identity_pair)?;
            let kyber = prekey_manager.generate_kyber_prekey(1, identity_pair)?;
            let one_time = prekey_manager.generate_prekeys(&mut rng, 1, 100)?;
            Ok((reg_id, signed, kyber, one_time))
        }
        let (registration_id, signed, kyber, one_time) =
            gen_keys(&prekey_manager, &identity_pair)?;

        // Identity-overwrite guard. If a local identity already exists,
        // refuse to import a *different* one — overwriting would silently
        // break every established session on this device. Same identity is
        // idempotent (lets a user retry a half-completed link).
        let identity_pair_serialized = identity_pair.serialize();
        if let Some((_existing_reg, existing_pair)) =
            IdentityKeyManager::new(self.key_store.clone()).get_identity()?
        {
            let existing_serialized = existing_pair.serialize();
            if existing_serialized.as_ref() != identity_pair_serialized.as_ref() {
                return Err(LinkDeviceError::Invalid(
                    "local identity already exists and differs from incoming \
                     ProvisionMessage — refuse to overwrite (sign out + clear \
                     local data first if you really mean to re-link)"
                        .into(),
                ));
            }
        }

        // NOTE: identity import is *deferred* until the server confirms the
        // device-linking RPC succeeded. Any failure between here and
        // `complete_device_linking` returning Ok leaves local key_store +
        // settings untouched, so a retry can proceed cleanly. Prekey
        // generation above is in-memory only.

        // Profile key + auth token (server-side token is bootstrapped via
        // device_credentials::generate; the server also expects it in
        // CompleteDeviceLinkingRequest, supplied below).
        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&provision.profile_key);
        let credentials = DeviceCredentials::generate(/* placeholder */ 1, &profile_key);
        let device_auth_token = credentials.auth_token().as_bytes();
        let uak = crate::crypto::device_credentials::derive_uak(&profile_key);

        use crate::proto::chat_v1::signal_pre_key_bundle::{
            KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord as ProtoPreKey,
            SignedPreKeyRecord as ProtoSignedPreKey,
        };
        use crate::proto::chat_v1::SignalPreKeyBundle;
        use libsignal_protocol::GenericSignedPreKey;

        let signed_id: u32 = signed
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let signed_pub = signed
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize()
            .to_vec();
        let signed_sig = signed
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        let signed_ts = signed
            .timestamp()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .epoch_millis();

        let kyber_id: u32 = kyber
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let kyber_pub = kyber
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize()
            .to_vec();
        let kyber_sig = kyber
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        let identity_pub_full = identity_pair.identity_key().serialize().to_vec();

        let one_time_first = if let Some(pk) = one_time.first() {
            let id: u32 = pk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let pub_bytes = pk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize()
                .to_vec();
            Some(ProtoPreKey {
                id,
                public_key: pub_bytes,
            })
        } else {
            None
        };

        let prekey_bundle = SignalPreKeyBundle {
            identity_key: identity_pub_full.clone(),
            registration_id,
            device_id: 0, // server assigns
            signed_prekey: Some(ProtoSignedPreKey {
                id: signed_id,
                public_key: signed_pub,
                signature: signed_sig,
                timestamp: signed_ts,
            }),
            one_time_prekey: one_time_first,
            kyber_prekey: Some(ProtoKyberPreKey {
                id: kyber_id,
                public_key: kyber_pub,
                signature: kyber_sig,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default(),
            }),
            unidentified_access_key: Some(uak.to_vec()),
            unrestricted_unidentified_access: Some(false),
        };

        let mut initial_one_time: Vec<ProtoPreKey> = Vec::new();
        for pk in one_time.iter().skip(1) {
            let id: u32 = pk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let pub_bytes = pk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize()
                .to_vec();
            initial_one_time.push(ProtoPreKey {
                id,
                public_key: pub_bytes,
            });
        }

        let mut client = self.provisioning_client()?;
        let response = client
            .complete_device_linking(
                &provision.provisioning_code,
                device_auth_token,
                &uak,
                &session.device_info,
                &session.device_name,
                prekey_bundle,
                initial_one_time,
            )
            .await?;

        let device_id = response.device_id;

        // Server confirmed — NOW commit identity + auth state. Earlier
        // failure modes (network, validation, server reject) leave the
        // local key store unmodified so a retry starts from clean state.
        self.key_store
            .set_local_identity(registration_id, &identity_pair_serialized)?;

        // Persist auth state.
        self.settings_repo
            .set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo.set(keys::CURRENT_DEVICE_ID, &device_id)?;
        self.settings_repo
            .set_bytes(keys::PROFILE_KEY, &profile_key)?;
        self.settings_repo
            .set_bytes(keys::DEVICE_AUTH_TOKEN, device_auth_token)?;
        self.settings_repo
            .set_bytes(keys::IDENTITY_KEY_PUBLIC, &identity_pub_full)?;

        let display_name = response
            .user_profile
            .as_ref()
            .map(|p| p.display_name.clone())
            .filter(|s| !s.is_empty());
        if let Some(ref name) = display_name {
            self.settings_repo.set_string(keys::DISPLAY_NAME, name)?;
        }

        // Configure auth interceptor for Basic auth.
        self.grpc_pool.auth().set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        Ok(CompletedLink {
            user_id,
            device_id,
            display_name,
        })
    }

    // ====================================================================
    // Primary side (authenticated)
    // ====================================================================

    /// Parse a `chatter://link?uuid=…&pub_key=…&server=…&name=…` URL.
    pub fn parse_link(url: &str) -> Result<LinkRequest, LinkDeviceError> {
        let parsed =
            Url::parse(url).map_err(|e| LinkDeviceError::InvalidLinkUrl(e.to_string()))?;
        if parsed.scheme() != LINK_URL_SCHEME {
            return Err(LinkDeviceError::InvalidLinkUrl(format!(
                "expected scheme {LINK_URL_SCHEME}, got {}",
                parsed.scheme()
            )));
        }
        if parsed.host_str() != Some(LINK_URL_HOST) {
            return Err(LinkDeviceError::InvalidLinkUrl(format!(
                "expected host {LINK_URL_HOST}, got {:?}",
                parsed.host_str()
            )));
        }

        let mut uuid = None;
        let mut pub_key_b64 = None;
        let mut server_url = None;
        let mut device_name = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "uuid" => uuid = Some(v.to_string()),
                "pub_key" => pub_key_b64 = Some(v.to_string()),
                "server" => server_url = Some(v.to_string()),
                "name" => device_name = Some(v.to_string()),
                _ => {}
            }
        }

        let uuid = uuid.ok_or_else(|| LinkDeviceError::InvalidLinkUrl("missing uuid".into()))?;
        let pub_key_b64 =
            pub_key_b64.ok_or_else(|| LinkDeviceError::InvalidLinkUrl("missing pub_key".into()))?;
        let pub_key_bytes = base64_url_decode(&pub_key_b64)
            .map_err(|e| LinkDeviceError::InvalidLinkUrl(format!("pub_key decode: {e}")))?;
        if pub_key_bytes.len() != 32 {
            return Err(LinkDeviceError::InvalidLinkUrl(format!(
                "pub_key must be 32 bytes, got {}",
                pub_key_bytes.len()
            )));
        }
        let mut peer_pub = [0u8; 32];
        peer_pub.copy_from_slice(&pub_key_bytes);

        Ok(LinkRequest {
            uuid,
            peer_pub,
            server_url,
            device_name,
        })
    }

    /// Approve a link request. **Caller MUST gate this on a re-auth check
    /// via [`verify_password`]** — this function does not re-prompt the user.
    ///
    /// Builds a [`ProvisionMessage`] from the local identity, profile key,
    /// and registration ID; encrypts it for the new device's pubkey;
    /// uploads via `provision_device`. The new device picks it up via its
    /// poll loop.
    pub async fn approve_link(&self, req: &LinkRequest) -> Result<(), LinkDeviceError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(LinkDeviceError::NotLoggedIn)?;

        let identity_manager = IdentityKeyManager::new(self.key_store.clone());
        let (registration_id, identity_pair) = identity_manager
            .get_identity()?
            .ok_or(CryptoError::NoIdentity)?;

        let priv_bytes = identity_pair.private_key().serialize().to_vec();

        let profile_key = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .ok_or_else(|| LinkDeviceError::Invalid("profile key missing".into()))?;
        if profile_key.len() != 32 {
            return Err(LinkDeviceError::Invalid("profile key wrong length".into()));
        }

        // Server expects this code echoed back from new device in
        // CompleteDeviceLinking. Today server validation of the code is
        // limited (no out-of-band channel between server and primary), so
        // we treat it as a freshness marker and put a random value here.
        let provisioning_code = random_provisioning_code();

        let message = ProvisionMessage {
            aci_identity_key_private: priv_bytes,
            pni_identity_key_private: vec![],
            aci: user_id_str,
            pni: String::new(),
            number: String::new(),
            provisioning_code,
            profile_key,
            master_key: vec![],
            // TODO: pull these from settings once we expose them
            read_receipts: true,
            typing_indicators: true,
            link_previews: true,
            provisioning_user_agent: format!(
                "chatter-linux/{}",
                env!("CARGO_PKG_VERSION"),
            ),
            primary_device_registration_id: registration_id,
        };

        let envelope: ProvisionEnvelope = encrypt_envelope(&req.peer_pub, &message)?;

        let mut client = self.provisioning_client()?;
        // Send the same provisioning_code in cleartext alongside the envelope
        // — server stores it for `complete_device_linking` to validate when
        // the new device echoes it back from the decrypted envelope.
        client
            .provision_device(&req.uuid, envelope, &message.provisioning_code)
            .await?;
        Ok(())
    }

    pub async fn cancel(&self, uuid: &str) -> Result<(), LinkDeviceError> {
        let mut client = self.provisioning_client()?;
        client.cancel_provisioning(uuid).await?;
        Ok(())
    }

    pub async fn list_pending(
        &self,
    ) -> Result<Vec<ProvisioningSession>, LinkDeviceError> {
        let mut client = self.provisioning_client()?;
        let response = client.list_provisioning_sessions().await?;
        Ok(response.sessions)
    }

    /// Single-shot primary-side approve. Parses the URL, verifies the
    /// password against local profile material, then encrypts + uploads
    /// the provision envelope. UI passes raw user inputs and gets back a
    /// summary of what was just linked.
    ///
    /// Emits [`DomainEvent::DeviceLinked`] (with `approved_by_self = true`)
    /// on success so any open Devices modal / toast banner can react.
    pub async fn approve_link_from_url(
        &self,
        url: &str,
        secret: &str,
    ) -> Result<ApprovedDevice, LinkDeviceError> {
        let req = Self::parse_link(url)?;
        if !self.verify_password_or_pin(secret)? {
            return Err(LinkDeviceError::ReauthFailed);
        }
        self.approve_link(&req).await?;

        // Server doesn't return the new device_id at provision_device time
        // (that's known only after the new device calls complete_device_linking).
        // Use 0 as a placeholder; the Devices modal will refresh from
        // ListDevices on the event handler.
        self.event_bus.emit_domain(DomainEvent::DeviceLinked {
            device_id: 0,
            device_name: req.device_name.clone(),
            device_info: None,
            approved_by_self: true,
        });

        // OS toast — quietly best-effort, don't fail the approval if the
        // platform notification daemon is missing.
        let title = "New device linked";
        let body = match &req.device_name {
            Some(name) if !name.is_empty() => format!("Linked: {name}"),
            _ => "A new device just joined your account.".to_string(),
        };
        if let Err(e) = self
            .notifications
            .show_message(title, &body, NotificationPriority::Normal)
        {
            tracing::warn!(error = %e, "DeviceLinked OS notification failed");
        }

        Ok(ApprovedDevice {
            uuid: req.uuid,
            device_name: req.device_name,
            server_url: req.server_url,
        })
    }

    /// UI-side preview: parse the URL only, no network, no password.
    /// Lets the modal show "About to link Alex's Laptop" before the user
    /// types a password.
    pub fn preview_link(url: &str) -> Result<ApprovedDevice, LinkDeviceError> {
        let req = Self::parse_link(url)?;
        Ok(ApprovedDevice {
            uuid: req.uuid,
            device_name: req.device_name,
            server_url: req.server_url,
        })
    }
}

/// Summary of a just-approved (or previewed) link request, for UI display.
#[derive(Debug, Clone)]
pub struct ApprovedDevice {
    pub uuid: String,
    pub device_name: Option<String>,
    pub server_url: Option<String>,
}

/// Drive the full new-device flow: start → render QR → poll → complete.
/// Emits progress on `tx`. Terminal event is always `Linked`, `Failed`, or
/// `Cancelled`. Cleans up `active_uuid` before returning regardless.
async fn run_flow(
    svc: Arc<LinkDeviceService>,
    device_info: String,
    device_name: String,
    tx: mpsc::UnboundedSender<LinkProgress>,
) {
    // 1. Start session + render QR.
    let session = match svc.start_as_new_device(&device_info, &device_name).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(LinkProgress::Failed(format!("start failed: {e}")));
            return;
        }
    };
    let uuid = session.uuid.clone();
    let url = session.qr_url.clone();
    let expires_at = session.expires_at;

    let qr_png = match qr_png_for_link_url(&url, 512) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(LinkProgress::Failed(format!("QR render: {e}")));
            return;
        }
    };

    *svc.active_uuid.lock() = Some(uuid.clone());

    let _ = tx.send(LinkProgress::QrReady {
        url,
        qr_png,
        expires_at,
    });
    let _ = tx.send(LinkProgress::WaitingForApproval);

    // 2. Poll for envelope.
    let provision = match svc.poll_until_envelope(&session).await {
        Ok(p) => p,
        Err(LinkDeviceError::SessionEnded(msg)) => {
            *svc.active_uuid.lock() = None;
            let _ = tx.send(LinkProgress::Cancelled);
            tracing::info!(reason = %msg, "Provisioning session ended");
            return;
        }
        Err(e) => {
            *svc.active_uuid.lock() = None;
            let _ = tx.send(LinkProgress::Failed(format!("poll failed: {e}")));
            return;
        }
    };

    *svc.active_uuid.lock() = None;
    let _ = tx.send(LinkProgress::Approved);

    // 3. Complete linking.
    match svc.complete_linking(session, provision).await {
        Ok(c) => {
            let _ = tx.send(LinkProgress::Linked(c));
        }
        Err(e) => {
            let _ = tx.send(LinkProgress::Failed(format!("complete failed: {e}")));
        }
    }
}

/// Render a `chatter://link?…` URL as a square PNG QR code.
///
/// `size_px` is clamped to `[128, 1024]`. Returned bytes are a complete
/// PNG image suitable for `slint::Image::load_from_memory` or saving to disk.
pub fn qr_png_for_link_url(url: &str, size_px: u32) -> Result<Vec<u8>, LinkDeviceError> {
    use image::{ImageEncoder, Luma};
    use qrcode::QrCode;

    let size = size_px.clamp(128, 1024);
    let qr = QrCode::new(url.as_bytes())
        .map_err(|e| LinkDeviceError::Invalid(format!("QR generate: {e}")))?;
    let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();
    let scaled = image::imageops::resize(
        &qr_image,
        size,
        size,
        image::imageops::FilterType::Nearest,
    );
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
        .map_err(|e| LinkDeviceError::Invalid(format!("PNG encode: {e}")))?;
    Ok(png)
}

// ============================================================================
// Helpers
// ============================================================================

fn base64_url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_url_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.decode(s)
}

fn build_link_url(
    uuid: &str,
    pub_key_b64: &str,
    server_url: Option<&str>,
    device_name: Option<&str>,
) -> String {
    let mut query =
        url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("uuid", uuid);
    query.append_pair("pub_key", pub_key_b64);
    if let Some(server) = server_url {
        query.append_pair("server", server);
    }
    if let Some(name) = device_name {
        query.append_pair("name", name);
    }
    format!(
        "{LINK_URL_SCHEME}://{LINK_URL_HOST}?{}",
        query.finish()
    )
}

fn random_provisioning_code() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: [u8; 24] = std::array::from_fn(|_| rng.random());
    base64_url(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_round_trip_link_url() {
        let pub_bytes = [0xAB; 32];
        let pub_b64 = base64_url(&pub_bytes);
        let url = build_link_url(
            "abc-123",
            &pub_b64,
            Some("https://chat.example.com"),
            Some("Alex's Laptop"),
        );

        let req = LinkDeviceService::parse_link(&url).expect("parse");
        assert_eq!(req.uuid, "abc-123");
        assert_eq!(req.peer_pub, pub_bytes);
        assert_eq!(req.server_url.as_deref(), Some("https://chat.example.com"));
        assert_eq!(req.device_name.as_deref(), Some("Alex's Laptop"));
    }

    #[test]
    fn rejects_wrong_scheme() {
        let err = LinkDeviceService::parse_link("https://link?uuid=x&pub_key=y").expect_err("bad");
        assert!(matches!(err, LinkDeviceError::InvalidLinkUrl(_)));
    }

    #[test]
    fn rejects_wrong_host() {
        let err = LinkDeviceService::parse_link("chatter://join?uuid=x&pub_key=y")
            .expect_err("bad");
        assert!(matches!(err, LinkDeviceError::InvalidLinkUrl(_)));
    }

    #[test]
    fn rejects_short_pubkey() {
        let pub_b64 = base64_url(&[0u8; 16]);
        let url = build_link_url("u", &pub_b64, None, None);
        let err = LinkDeviceService::parse_link(&url).expect_err("bad");
        assert!(matches!(err, LinkDeviceError::InvalidLinkUrl(_)));
    }

    #[test]
    fn random_code_is_unique() {
        let a = random_provisioning_code();
        let b = random_provisioning_code();
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }

    // ========================================================================
    // verify_password / verify_pin / verify_password_or_pin
    // ========================================================================

    use crate::crypto::auth_crypto::{derive_profile_key, hash_password};
    use crate::crypto::svr::derive_pin_fingerprint;
    use crate::domain::ids::UserId;
    use crate::domain::user::User;
    use crate::events::EventBus;
    use crate::network::grpc::client_pool::GrpcClientPool;
    use crate::services::notification_service::NotificationService;
    use crate::storage::repositories::user_repo::UserRepository;
    use crate::storage::{Database, KeyStore};
    use tempfile::NamedTempFile;

    /// Build a fully-initialised LinkDeviceService backed by a fresh
    /// in-memory database, registered with the given username + (optional)
    /// password + (optional) PIN. Returns the service plus a kept-alive
    /// tempfile so the SQLite-backed DB survives.
    fn setup_service(
        username: &str,
        password: Option<&str>,
        pin: Option<&str>,
    ) -> (Arc<LinkDeviceService>, NamedTempFile) {
        let db_file = NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();

        let key_store = Arc::new(KeyStore::new(db.clone()));
        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));
        let user_repo = Arc::new(UserRepository::new(db.clone()));

        let user_id: UserId = "11111111-1111-1111-1111-111111111111".parse().unwrap();
        settings_repo
            .set_string(keys::CURRENT_USER_ID, &user_id.to_string())
            .unwrap();

        // Insert the user row so verify_password / verify_pin can look up the
        // username for the derivation salt.
        let user = User {
            id: user_id,
            username: username.to_string(),
            svr_public_key: Vec::new(),
            last_active: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
        };
        user_repo.insert(&user).unwrap();

        if let Some(pw) = password {
            let pw_hash = hash_password(pw);
            let profile_key = derive_profile_key(&pw_hash, username).unwrap();
            settings_repo
                .set_bytes(keys::PROFILE_KEY, &profile_key)
                .unwrap();
        }
        if let Some(p) = pin {
            let fp = derive_pin_fingerprint(p, username).unwrap();
            settings_repo
                .set_bytes(keys::PIN_FINGERPRINT, &fp)
                .unwrap();
        }

        // Network/event/notification deps aren't exercised by verify_*; build
        // smallest possible instances.
        let grpc_pool = Arc::new(
            GrpcClientPool::new("http://127.0.0.1:0").expect("grpc pool"),
        );
        let event_bus = EventBus::new();
        let notifications = Arc::new(NotificationService::noop());

        let svc = Arc::new(LinkDeviceService::new(
            grpc_pool,
            settings_repo,
            user_repo,
            key_store,
            event_bus,
            notifications,
        ));
        (svc, db_file)
    }

    #[test]
    fn verify_password_accepts_correct_password() {
        let (svc, _db) = setup_service("alice", Some("hunter2"), None);
        assert_eq!(svc.verify_password("hunter2").unwrap(), true);
    }

    #[test]
    fn verify_password_rejects_wrong_password() {
        let (svc, _db) = setup_service("alice", Some("hunter2"), None);
        assert_eq!(svc.verify_password("wrong-pw").unwrap(), false);
    }

    #[test]
    fn verify_password_oidc_user_returns_unsupported() {
        // No PROFILE_KEY persisted — simulates OIDC session.
        let (svc, _db) = setup_service("alice", None, None);
        let err = svc.verify_password("anything").unwrap_err();
        assert!(matches!(err, LinkDeviceError::ReauthUnsupportedForOidc));
    }

    #[test]
    fn verify_pin_accepts_correct_pin() {
        let (svc, _db) = setup_service("alice", None, Some("123456"));
        assert_eq!(svc.verify_pin("123456").unwrap(), true);
    }

    #[test]
    fn verify_pin_rejects_wrong_pin() {
        let (svc, _db) = setup_service("alice", None, Some("123456"));
        assert_eq!(svc.verify_pin("000000").unwrap(), false);
    }

    #[test]
    fn verify_pin_format_validation() {
        let (svc, _db) = setup_service("alice", None, Some("123456"));
        assert!(matches!(
            svc.verify_pin("12345").unwrap_err(),
            LinkDeviceError::Invalid(_)
        ));
        assert!(matches!(
            svc.verify_pin("12345a").unwrap_err(),
            LinkDeviceError::Invalid(_)
        ));
    }

    #[test]
    fn verify_pin_no_fingerprint_errors() {
        // Password-only account, no PIN persisted.
        let (svc, _db) = setup_service("alice", Some("hunter2"), None);
        assert!(matches!(
            svc.verify_pin("123456").unwrap_err(),
            LinkDeviceError::Invalid(_)
        ));
    }

    #[test]
    fn verify_password_or_pin_routes_pin_for_six_digits() {
        let (svc, _db) = setup_service("alice", Some("hunter2"), Some("123456"));
        // 6-digit input => PIN path
        assert_eq!(svc.verify_password_or_pin("123456").unwrap(), true);
        // longer input => password path
        assert_eq!(svc.verify_password_or_pin("hunter2").unwrap(), true);
    }

    #[test]
    fn verify_password_or_pin_oidc_pin_works() {
        // OIDC session (no password) but PIN configured.
        let (svc, _db) = setup_service("alice", None, Some("987654"));
        assert_eq!(svc.verify_password_or_pin("987654").unwrap(), true);
    }

    #[test]
    fn verify_password_or_pin_oidc_password_attempt_returns_false() {
        let (svc, _db) = setup_service("alice", None, Some("987654"));
        // Long string => password path => OIDC unsupported gets folded to false.
        assert_eq!(svc.verify_password_or_pin("anything-long").unwrap(), false);
    }
}
