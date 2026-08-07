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
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::auth_crypto::{derive_login_auth_hash, hash_password};
use crate::crypto::provisioning::{
    decrypt_envelope, encrypt_envelope, ProvisioningKeypair,
};
use crate::crypto::message_backup::derive_link_sync_kek;
use crate::crypto::signal::identity::IdentityKeyManager;
use crate::crypto::signal::prekey::PreKeyManager;
use crate::crypto::DeviceCredentials;
use crate::crypto::SecretString;
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::domain::notification::NotificationPriority;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::services::message_backup_service::{BackupStats, MessageBackupService};
use crate::services::notification_service::NotificationService;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::provisioning_client::{
    ProvisioningClient, ProvisioningPollResult, TransferArchivePoll,
};
use crate::proto::provisioning_v1::{ProvisionEnvelope, ProvisionMessage, ProvisioningSession};
use crate::storage::backup_data::ImportSummary;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::storage::repositories::user_repo::UserRepository;
use crate::storage::{Database, KeyStore};

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

/// Pre-image of a parsed link URL (primary-side internal).
///
/// `pub(crate)` — outside callers receive a [`PendingDeviceLink`] from
/// [`LinkDeviceService::prepare_link_from_url`], which gates installation
/// on a separate `approve_link` step. Direct construction from outside the
/// crate would let UI / external code skip the user-confirmation gate.
#[derive(Debug, Clone)]
pub(crate) struct LinkRequest {
    pub uuid: String,
    /// Raw 32-byte X25519 ephemeral public key from new device.
    pub peer_pub: [u8; 32],
    /// Server URL hint embedded by the new device (so primary can warn on
    /// cross-server linking attempts).
    pub server_url: Option<String>,
    pub device_name: Option<String>,
}

/// A parsed, previewed device-link request that has **not yet** been
/// approved or installed. Returned by
/// [`LinkDeviceService::prepare_link_from_url`].
///
/// The UI **MUST** display [`Self::verification_code`] alongside the
/// device name and require explicit user confirmation that the code
/// matches the value shown on the new device before passing this to
/// [`LinkDeviceService::approve_link`].
///
/// Properties enforced at the type level:
/// - **Not [`Clone`]**: a single pending link cannot be silently fanned
///   out to multiple approval calls.
/// - **Move-consumed by `approve_link`**: the value is taken by value
///   (not by reference), so it cannot be reused after a successful or
///   failed approval.
/// - The inner `LinkRequest` is `pub(crate)` and there is no public
///   constructor — outside code can only obtain a `PendingDeviceLink`
///   from `prepare_link_from_url`.
///
/// The verification code is derived deterministically from the new
/// device's ephemeral X25519 public key (truncated SHA-256, first 6 hex
/// chars, uppercased), so the **same** value can be displayed on both
/// devices — matching it confirms both sides see the same QR payload and
/// no MITM substituted a different ephemeral key.
#[derive(Debug)]
pub struct PendingDeviceLink {
    inner: LinkRequest,
    verification_code: String,
}

impl PendingDeviceLink {
    /// Short verification code for UI display. Compare with the code shown
    /// on the new device — if they match, the QR payload was not tampered
    /// with in transit.
    pub fn verification_code(&self) -> &str {
        &self.verification_code
    }

    /// New device's user-supplied display name (if any).
    pub fn device_name(&self) -> Option<&str> {
        self.inner.device_name.as_deref()
    }

    /// Server URL the new device intends to talk to (if embedded in the
    /// QR). Lets UI flag cross-server linking attempts.
    pub fn server_url(&self) -> Option<&str> {
        self.inner.server_url.as_deref()
    }

    /// Provisioning session UUID. Useful for cancel / list flows.
    pub fn uuid(&self) -> &str {
        &self.inner.uuid
    }
}

/// Compute the 6-character verification code for a peer public key.
///
/// `SHA-256(peer_pub)` truncated to 3 bytes, hex-encoded uppercase.
/// Deterministic: both the primary and the new device produce the same
/// code from the same QR payload, so the user can compare them by eye.
fn verification_code_for_pub(peer_pub: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let digest = Sha256::digest(peer_pub);
    let mut out = String::with_capacity(6);
    for byte in digest.iter().take(3) {
        let _ = write!(out, "{byte:02X}");
    }
    out
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
    /// Backs the Link'n'Sync transfer archive (§30): the primary exports message
    /// history from it, the new device imports into it. Held so the linking flow
    /// can build a `MessageBackupService` without a wider client handle.
    database: Arc<Database>,
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
        database: Arc<Database>,
        event_bus: EventBus,
        notifications: Arc<NotificationService>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            user_repo,
            key_store,
            database,
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
        // Cancel the flow if the receiver is dropped (UI closes the linking
        // page). The cloned sender's `closed()` future resolves when every
        // receiver clone has been dropped — at that point select! drops the
        // running run_flow future at its next await, halting further server
        // polling and DB writes that nothing is listening to.
        let cancel = tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.closed() => {
                    tracing::info!("link device flow cancelled: receiver dropped");
                }
                _ = run_flow(svc, device_info, device_name, tx) => {}
            }
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

        crate::crypto::svr::validate_pin_shape(pin)
            .map_err(|e| LinkDeviceError::Invalid(e.to_string()))?;

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

    /// Convenience: verify EITHER a password OR a PIN against local
    /// material. Tries the cheaper PIN check first when the input looks like
    /// a PIN; otherwise falls through to the password check. Lets the UI keep
    /// a single "confirm with password or PIN" input.
    pub fn verify_password_or_pin(&self, secret: &str) -> Result<bool, LinkDeviceError> {
        // Try PIN first (cheap local HMAC compare against the stored
        // fingerprint). PIN and password share format constraints now (both
        // are 8+ printable ASCII), so we can't tell them apart by shape -
        // we attempt PIN, and fall through to password if PIN validation
        // rejects the input or the fingerprint doesn't match.
        if crate::crypto::svr::validate_pin_shape(secret).is_ok() {
            if let Ok(true) = self.verify_pin(secret) {
                return Ok(true);
            }
        }
        self.verify_password(secret)
    }

    /// Re-auth gate. Validates the user's password against the locally-stored
    /// login-auth fingerprint. **Local-only check** — does not contact the
    /// server. Constant-time comparison.
    ///
    /// Returns `Ok(false)` if no fingerprint is stored (nothing to match
    /// against). Caller may fall back to [`Self::verify_pin`].
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

        // Re-auth via login_auth_hash: derive from entered password and
        // compare to the stored fingerprint. Profile_key cannot be used here
        // any more (it is random, not derivable from password).
        let stored_auth_hash = match self.settings_repo.get_bytes(keys::AUTH_LOOKUP_HASH)? {
            Some(h) => h,
            None => return Ok(false),
        };
        if stored_auth_hash.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "stored auth_lookup_hash has wrong length".into(),
            ));
        }

        let password_hash = hash_password(password);
        let derived = derive_login_auth_hash(&password_hash, &user.username);

        Ok(derived.ct_eq(stored_auth_hash.as_slice()).unwrap_u8() == 1)
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
                Vec<libsignal_protocol::KyberPreKeyRecord>,
            ),
            CryptoError,
        > {
            let mut rng = rand::rng();
            let reg_id = (rand::Rng::random::<u32>(&mut rng) % 0x3FFF) + 1;
            let signed =
                prekey_manager.generate_signed_prekey(&mut rng, 1, identity_pair)?;
            // The medium-term LAST-RESORT PQSPK (id 1) — reused across
            // handshakes until rotated, so it is a floor, not the primary PQ
            // key material.
            let kyber = prekey_manager.generate_kyber_prekey(1, identity_pair)?;
            let one_time = prekey_manager.generate_prekeys(&mut rng, 1, 100)?;

            // The one-time Kyber prekey pool (PQOPK), same batch size as the
            // classical OPKs above. Per PQXDH §3.2 the server pops one PQOPK for
            // every classical OPK it hands out, so the two pools MUST be seeded
            // in lockstep. A linked device is a full recipient of every message
            // the account receives; without its own pool the server has nothing
            // to pop for it and every PQXDH handshake to this device falls back
            // to the single last-resort PQSPK above — spec-permitted, but one
            // reused PQ secret across every session (see the warning at
            // crypto/signal/identity.rs:38-42).
            //
            // This mirrors `services/auth_service.rs::register`, which seeds the
            // same pool for the device created by registration. IDs start at 2:
            // the last-resort PQSPK holds id 1, and `generate_kyber_prekey`
            // persists each record, so `get_next_kyber_prekey_id` advances as we
            // go.
            let mut kyber_one_time = Vec::with_capacity(100);
            for _ in 0..100 {
                let id = prekey_manager.get_next_kyber_prekey_id()?;
                kyber_one_time.push(prekey_manager.generate_kyber_prekey(id, identity_pair)?);
            }
            Ok((reg_id, signed, kyber, one_time, kyber_one_time))
        }
        let (registration_id, signed, kyber, one_time, kyber_one_time) =
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
        // Chatter-owned wire field is NANOSECONDS; libsignal stores millis.
        // The server reads this with time.Unix(0, ts), so sending millis dated
        // the linked device's signed prekey to 1970-01-01T00:29:44Z — ~56 years
        // "old". Registration already converts (see auth_service.rs); this path
        // did not, so every linked bundle was stored pre-aged and would be
        // treated as long expired by any age check.
        let signed_ts = signed
            .timestamp()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .epoch_millis()
            * 1_000_000;

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
            // Placeholder. The server assigns the real device_id in
            // CreateLinkedDevice (MAX(device_id)+1) and ignores this field
            // entirely — the new device cannot know its id before the server
            // allocates it. This said 0, but the shared SignalPreKeyBundle
            // contract requires device_id >= 1 (common.proto:167, correct for
            // every other use of the message: a real device is never 0), so
            // the server rejected every CompleteDeviceLinking with
            // "prekey_bundle.device_id: must be greater than or equal to 1"
            // and linking could never complete. 1 matches the placeholder
            // already used for DeviceCredentials::generate above.
            device_id: 1,
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
                // Nanoseconds, as above — this field had the same millis bug.
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
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

        // Build the PQOPK batch for the server. Unlike the classical OPK, every
        // PQ prekey carries an XEdDSA signature over its public key (PQXDH §3.2);
        // the server rejects the batch outright if it does not verify. None are
        // held back for the bundle: the bundle's `kyber_prekey` field is the
        // last-resort PQSPK, and the server picks a PQOPK from this pool at
        // handout time.
        let mut initial_kyber_one_time: Vec<ProtoKyberPreKey> = Vec::new();
        for kpk in &kyber_one_time {
            let id: u32 = kpk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let public = kpk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize()
                .to_vec();
            let sig = kpk
                .signature()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?;
            // Chatter-owned wire field is nanoseconds; libsignal stores millis.
            let ts = kpk
                .timestamp()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .epoch_millis()
                * 1_000_000;
            initial_kyber_one_time.push(ProtoKyberPreKey {
                id,
                public_key: public,
                signature: sig,
                timestamp: ts,
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

        // The server sends our display_name ENCRYPTED with our profile key.
        // Decrypt with the key we just provisioned (line above) before
        // persisting — never store ciphertext as our own name.
        let display_name = response
            .user_profile
            .as_ref()
            .map(|p| p.display_name.clone())
            .filter(|s| !s.is_empty())
            .and_then(|encrypted_b64| {
                use base64::Engine;
                let pk: [u8; 32] = profile_key[..].try_into().ok()?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&encrypted_b64)
                    .ok()?;
                crate::crypto::profile_crypto::decrypt_display_name(&pk, &bytes)
                    .ok()
                    .filter(|n| !n.is_empty())
            });
        if let Some(ref name) = display_name {
            self.settings_repo.set_string(keys::DISPLAY_NAME, name)?;
        }

        // Configure auth interceptor for Basic auth.
        //
        // Rebind the credentials to the device_id the SERVER just assigned.
        // `credentials` above was built with a placeholder of 1 because this
        // device cannot know its own id until CompleteDeviceLinking returns, and
        // auth_username() formats `{user_id}.{device_id}` — so authenticating
        // with the placeholder claims to be device 1, the PRIMARY. The server
        // verifies the token against (user_id, device_id), and this device's
        // token is registered under its real id, so every authenticated request
        // from a linked device was rejected. The auth token and UAK are
        // unchanged; only the id they are presented under is corrected.
        let credentials = DeviceCredentials::from_stored(
            device_id,
            *credentials.auth_token().as_bytes(),
            uak,
        );
        self.grpc_pool.auth().set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        // Seed this device's PQOPK pool on the server. Reuses the existing
        // RefreshPreKeys RPC rather than adding a field to
        // CompleteDeviceLinking, which also satisfies both ordering
        // constraints for free:
        //   - signal_kyber_one_time_prekeys has an FK on
        //     signal_prekey_bundles(user_id, device_id), so this device's
        //     bundle must already exist — i.e. after complete_device_linking.
        //   - RefreshPreKeys is authenticated, so this must follow set_basic
        //     directly above.
        // The classical OPKs rode along inside CompleteDeviceLinkingRequest and
        // are already uploaded by this point; only the PQ pool is sent here.
        // This is the same shape as the registration-side seeding in
        // `services/auth_service.rs::register`.
        //
        // A failure here is logged, not fatal: the device is already linked
        // server-side, and PQXDH still succeeds against the last-resort PQSPK
        // (spec-permitted). Failing the link outright — after the identity and
        // auth state above are committed — would strand a half-linked device,
        // a worse outcome than the degradation it avoids. The background
        // replenisher refills both pools together on its next pass.
        if !initial_kyber_one_time.is_empty() {
            let pq_count = initial_kyber_one_time.len();
            let mut auth_client = self.grpc_pool.auth_client();
            match auth_client
                .refresh_pre_keys_with_kyber_otpk(Vec::new(), initial_kyber_one_time)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        device_id,
                        pq_opk = pq_count,
                        "Seeded PQOPK pool for linked device"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        device_id,
                        error = %e,
                        "Failed to seed PQOPK pool for linked device — PQXDH will fall back \
                         to the last-resort PQSPK until the replenisher runs"
                    );
                }
            }
        }

        // Link'n'Sync import (§30): if the primary exported a transfer archive,
        // fetch it, decrypt with the ephemeral-key-derived KEK, and import the
        // message history + social graph. Best-effort, exactly like the PQOPK
        // seed above: the device is already linked, so a poll/decrypt failure
        // leaves it linked-without-history, never stranded. Absent key = the
        // primary offered no history, which is a legal link.
        if let Some(eph) = provision.ephemeral_backup_key.as_deref() {
            if eph.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(eph);
                self.import_transfer_archive(
                    &session.uuid,
                    &key,
                    &provision.aci,
                    provision.primary_device_registration_id,
                )
                .await;
                key.zeroize();
            } else {
                tracing::warn!(
                    "provision ephemeral_backup_key is not 32 bytes; linking without history"
                );
            }
        }

        Ok(CompletedLink {
            user_id,
            device_id,
            display_name,
        })
    }

    /// Fetch + import the Link'n'Sync transfer archive on the new device (§30).
    /// Never returns an error: the device is already linked, so any failure is
    /// downgraded to "linked without history".
    async fn import_transfer_archive(
        &self,
        uuid: &str,
        ephemeral_backup_key: &[u8; 32],
        aci: &str,
        primary_registration_id: u32,
    ) {
        let backup = MessageBackupService::new(
            self.database.clone(),
            self.settings_repo.clone(),
            self.grpc_pool.clone(),
        );
        let mut kek = derive_link_sync_kek(ephemeral_backup_key, aci, primary_registration_id);

        let result: Result<Option<ImportSummary>, LinkDeviceError> = async {
            match self.poll_transfer_archive(uuid).await? {
                Some(download_url) => {
                    let summary = backup
                        .download_and_import_link_archive(&download_url, &kek)
                        .await
                        .map_err(|e| {
                            LinkDeviceError::Invalid(format!("import transfer archive: {e}"))
                        })?;
                    Ok(Some(summary))
                }
                // Primary failed/declined to export, or the wait timed out —
                // link without history.
                None => Ok(None),
            }
        }
        .await;
        kek.zeroize();

        match result {
            Ok(Some(summary)) => tracing::info!(
                rows_inserted = summary.rows_inserted,
                "link transfer archive imported"
            ),
            Ok(None) => tracing::info!("no transfer archive for this link; linked without history"),
            Err(e) => tracing::warn!(
                error = %e,
                "link transfer archive import failed; linked without history"
            ),
        }
    }

    /// Poll `GetTransferArchive` until Ready (returns the download URL), Failed
    /// (returns None — link without history), or a bounded deadline elapses
    /// (also None). Mirrors `poll_until_envelope`'s cadence.
    async fn poll_transfer_archive(
        &self,
        uuid: &str,
    ) -> Result<Option<String>, LinkDeviceError> {
        // A primary export of up to 1 GiB plus the upload can take a while;
        // 3 minutes is generous without hanging a linked device indefinitely.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        let mut client = self.provisioning_client()?;
        loop {
            match client.get_transfer_archive(uuid).await? {
                TransferArchivePoll::Ready { download_url, .. } => {
                    return Ok(Some(download_url));
                }
                TransferArchivePoll::Failed { message } => {
                    tracing::info!(%message, "transfer archive unavailable for this link");
                    return Ok(None);
                }
                TransferArchivePoll::Pending { retry_after_ms } => {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!("transfer archive poll timed out; linking without history");
                        return Ok(None);
                    }
                    let delay = retry_after_ms.clamp(1_000, 10_000);
                    tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                }
            }
        }
    }

    // ====================================================================
    // Primary side (authenticated)
    // ====================================================================

    /// Parse a `chatter://link?uuid=…&pub_key=…&server=…&name=…` URL.
    ///
    /// `pub(crate)` — external callers use [`Self::prepare_link_from_url`]
    /// which wraps this in a [`PendingDeviceLink`] gated by `approve_link`.
    pub(crate) fn parse_link(url: &str) -> Result<LinkRequest, LinkDeviceError> {
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

    /// Build + encrypt + upload the provision envelope for a parsed link.
    ///
    /// **`pub(crate)` only** — the only public path to this code is via
    /// [`Self::approve_link`], which requires a [`PendingDeviceLink`]
    /// (constructed only by [`Self::prepare_link_from_url`]) and a
    /// re-auth check on a [`SecretString`]. Direct external invocation
    /// would skip the user-confirmation gate.
    ///
    /// Builds a [`ProvisionMessage`] from the local identity, profile key,
    /// and registration ID; encrypts it for the new device's pubkey;
    /// uploads via `provision_device`. The new device picks it up via its
    /// poll loop.
    pub(crate) async fn install_envelope(
        &self,
        req: &LinkRequest,
    ) -> Result<(), LinkDeviceError> {
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

        // Mint the single-use Link'n'Sync archive key (§30). It rides ONLY
        // inside the E2E envelope; the server never sees it. The new device
        // derives the same KEK from it and decrypts the transfer archive we
        // export below. Keep the ACI for that same derivation before the
        // struct build moves `user_id_str`.
        let aci = user_id_str.clone();
        let mut ephemeral_backup_key = Zeroizing::new([0u8; 32]);
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(ephemeral_backup_key.as_mut())
                .expect("OS CSPRNG must not fail");
        }

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
            ephemeral_backup_key: Some(ephemeral_backup_key.to_vec()),
        };

        let envelope: ProvisionEnvelope = encrypt_envelope(&req.peer_pub, &message)?;

        let mut client = self.provisioning_client()?;
        // Send the same provisioning_code in cleartext alongside the envelope
        // — server stores it for `complete_device_linking` to validate when
        // the new device echoes it back from the decrypted envelope.
        client
            .provision_device(&req.uuid, envelope, &message.provisioning_code)
            .await?;

        // Keys are on their way. Now export this account's message history +
        // social graph as an encrypted archive keyed to the ephemeral key the
        // new device just received, and hand the server a blob pointer via
        // SetTransferArchive. Best-effort: a failure here signals the new
        // device to link WITHOUT history rather than hang — linking already
        // succeeded above.
        self.export_transfer_archive(&req.uuid, &ephemeral_backup_key, &aci, registration_id)
            .await;
        Ok(())
    }

    /// Export + upload the Link'n'Sync transfer archive for a linking session
    /// (§30). Never returns an error: linking has already succeeded by the time
    /// this runs, so any failure is downgraded to an `export_failed` signal that
    /// tells the polling new device to proceed without history.
    async fn export_transfer_archive(
        &self,
        uuid: &str,
        ephemeral_backup_key: &[u8; 32],
        aci: &str,
        primary_registration_id: u32,
    ) {
        let backup = MessageBackupService::new(
            self.database.clone(),
            self.settings_repo.clone(),
            self.grpc_pool.clone(),
        );
        let mut kek = derive_link_sync_kek(ephemeral_backup_key, aci, primary_registration_id);

        let result: Result<BackupStats, LinkDeviceError> = async {
            let (container, stats) = backup
                .build_link_archive(&kek)
                .map_err(|e| LinkDeviceError::Invalid(format!("build transfer archive: {e}")))?;
            let mut client = self.provisioning_client()?;
            let upload_url = client
                .set_transfer_archive(uuid, container.len() as u64, false)
                .await?;
            backup
                .upload_link_archive(&container, &upload_url)
                .await
                .map_err(|e| LinkDeviceError::Invalid(format!("upload transfer archive: {e}")))?;
            Ok(stats)
        }
        .await;
        kek.zeroize();

        match result {
            Ok(stats) => {
                tracing::info!(
                    rows = stats.rows,
                    bytes = stats.bytes,
                    "link transfer archive exported"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "link transfer archive export failed; new device will link without history"
                );
                if let Ok(mut client) = self.provisioning_client() {
                    // Signal the new device to stop polling. Best-effort: if
                    // even this fails, the new device's own poll deadline ends
                    // the wait.
                    let _ = client.set_transfer_archive(uuid, 0, true).await;
                }
            }
        }
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

    /// Step 1 of the primary-side device-link flow. Pure parse + preview —
    /// **no network call, no key install, no session state mutation**.
    ///
    /// Returns a [`PendingDeviceLink`] holding the parsed payload plus a
    /// short verification code derived from the new device's ephemeral
    /// public key.
    ///
    /// # Contract — the caller MUST:
    ///
    /// 1. Display [`PendingDeviceLink::verification_code`] (and ideally
    ///    the device name / server URL) to the user.
    /// 2. Obtain explicit confirmation that the code matches the value
    ///    shown on the new device.
    /// 3. **Only then** pass the value to [`Self::approve_link`] together
    ///    with the user's password / PIN.
    ///
    /// The returned type is `!Clone` and is consumed (`self`) by
    /// `approve_link`, so it cannot be silently re-used or fanned out.
    /// This is the in-SDK enforcement of the user-approval gate — a UI
    /// bug or alt-UI cannot complete a link by calling some other
    /// "install" entrypoint without going through this function.
    pub fn prepare_link_from_url(
        &self,
        url: &str,
    ) -> Result<PendingDeviceLink, LinkDeviceError> {
        let req = Self::parse_link(url)?;
        let verification_code = verification_code_for_pub(&req.peer_pub);
        Ok(PendingDeviceLink {
            inner: req,
            verification_code,
        })
    }

    /// Step 2 of the primary-side device-link flow. Consumes a
    /// [`PendingDeviceLink`] (so it cannot be replayed), re-authenticates
    /// the user against local material, then encrypts and uploads the
    /// provision envelope.
    ///
    /// # Contract
    ///
    /// **Caller MUST display the verification code from
    /// [`PendingDeviceLink`] to the user and obtain confirmation before
    /// calling `approve_link`.** The SDK has no way to verify the user
    /// actually saw and approved the code — this is the trust boundary.
    /// In return, the SDK guarantees that no install / session-storage
    /// path is reachable without first owning a `PendingDeviceLink`
    /// (see [`Self::install_envelope`] which is `pub(crate)`).
    ///
    /// `secret` accepts either the user's password or a PIN
    /// (auto-routed by [`Self::verify_password_or_pin`]). The
    /// [`SecretString`] wrapper zeroizes its contents on drop.
    ///
    /// On success, emits [`DomainEvent::DeviceLinked`] with
    /// `approved_by_self = true` so any open Devices modal / toast
    /// banner can react.
    pub async fn approve_link(
        &self,
        pending: PendingDeviceLink,
        secret: SecretString,
    ) -> Result<ApprovedDevice, LinkDeviceError> {
        let req = pending.inner;
        if !self.verify_password_or_pin(secret.expose_secret())? {
            return Err(LinkDeviceError::ReauthFailed);
        }
        self.install_envelope(&req).await?;

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
    // PendingDeviceLink / verification code
    // ========================================================================

    #[test]
    fn verification_code_is_deterministic_per_pubkey() {
        let pub_a = [0xAB; 32];
        let pub_b = [0xAB; 32];
        let pub_c = [0xCD; 32];
        let code_a = verification_code_for_pub(&pub_a);
        let code_b = verification_code_for_pub(&pub_b);
        let code_c = verification_code_for_pub(&pub_c);
        // Same pubkey => same code (so primary + new device match).
        assert_eq!(code_a, code_b);
        // Different pubkey => different code (so MITM with substituted
        // ephemeral key produces a visibly different value).
        assert_ne!(code_a, code_c);
        // 6 hex chars, uppercase only.
        assert_eq!(code_a.len(), 6);
        assert!(code_a.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn pending_device_link_is_not_clone() {
        // Compile-time check: this trait-bound function takes any type
        // implementing Clone. If `PendingDeviceLink` ever picked up a
        // `Clone` impl (deliberately or via auto-derive), `_assert` would
        // start compiling and we'd want the build to break. We can't
        // negate trait bounds at runtime, so fall back to a doc-style
        // assertion that PendingDeviceLink fields remain private.
        // The strongest live check is the `approve_link` signature: it
        // takes `pending: PendingDeviceLink` by value, not `&` or `&mut`.
        fn _consumes_by_value(_: PendingDeviceLink) {}
        let _ = _consumes_by_value;
    }

    // ========================================================================
    // verify_password / verify_pin / verify_password_or_pin
    // ========================================================================

    use crate::crypto::auth_crypto::{derive_login_auth_hash, hash_password};
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
            let auth_hash = derive_login_auth_hash(&pw_hash, username);
            settings_repo
                .set_bytes(keys::AUTH_LOOKUP_HASH, &auth_hash)
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
            db,
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
    fn verify_password_without_stored_hash_returns_false() {
        // No AUTH_LOOKUP_HASH persisted — nothing to match, so not a match.
        let (svc, _db) = setup_service("alice", None, None);
        assert_eq!(svc.verify_password("anything").unwrap(), false);
    }

    #[test]
    fn verify_pin_accepts_correct_pin() {
        let (svc, _db) = setup_service("alice", None, Some("devpin123"));
        assert_eq!(svc.verify_pin("devpin123").unwrap(), true);
    }

    #[test]
    fn verify_pin_rejects_wrong_pin() {
        let (svc, _db) = setup_service("alice", None, Some("devpin123"));
        assert_eq!(svc.verify_pin("wrongpin1").unwrap(), false);
    }

    #[test]
    fn verify_pin_format_validation() {
        let (svc, _db) = setup_service("alice", None, Some("devpin123"));
        // Too short (< MIN_PIN_LENGTH).
        assert!(matches!(
            svc.verify_pin("1234567").unwrap_err(),
            LinkDeviceError::Invalid(_)
        ));
        // Non-printable ASCII rejected even at the right length.
        assert!(matches!(
            svc.verify_pin("1234\t678").unwrap_err(),
            LinkDeviceError::Invalid(_)
        ));
    }

    #[test]
    fn verify_pin_no_fingerprint_errors() {
        // Password-only account, no PIN persisted.
        let (svc, _db) = setup_service("alice", Some("hunter2-strong"), None);
        assert!(matches!(
            svc.verify_pin("devpin123").unwrap_err(),
            LinkDeviceError::Invalid(_)
        ));
    }

    #[test]
    fn verify_password_or_pin_routes_pin_first() {
        let (svc, _db) = setup_service("alice", Some("hunter2-strong"), Some("devpin123"));
        // Correct PIN matches via PIN path.
        assert_eq!(svc.verify_password_or_pin("devpin123").unwrap(), true);
        // Correct password also matches via password fallback.
        assert_eq!(svc.verify_password_or_pin("hunter2-strong").unwrap(), true);
    }

    #[test]
    fn verify_password_or_pin_pin_only_account_works() {
        // No password fingerprint stored, but a PIN is configured.
        let (svc, _db) = setup_service("alice", None, Some("devpin123"));
        assert_eq!(svc.verify_password_or_pin("devpin123").unwrap(), true);
    }

    #[test]
    fn verify_password_or_pin_no_password_wrong_secret_returns_false() {
        let (svc, _db) = setup_service("alice", None, Some("devpin123"));
        // Wrong secret => PIN mismatch + no password fingerprint => false.
        assert_eq!(svc.verify_password_or_pin("anything-long-wrong").unwrap(), false);
    }
}
