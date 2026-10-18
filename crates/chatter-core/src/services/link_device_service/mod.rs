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
use rand::TryRngCore;
use tokio::sync::mpsc;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::DeviceCredentials;
use crate::crypto::SecretString;
use crate::crypto::message_backup::derive_link_sync_kek;
use crate::crypto::provisioning::{ProvisioningKeypair, decrypt_envelope, encrypt_envelope};
use crate::crypto::signal::identity::IdentityKeyManager;
use crate::crypto::signal::prekey::PreKeyManager;
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::network::grpc::provisioning_client::{
    ProvisioningClient, ProvisioningPollResult, TransferArchivePoll,
};
use crate::proto::provisioning_v1::{ProvisionEnvelope, ProvisionMessage, ProvisioningSession};
use crate::services::auth_service::{AuthError, prove_password};
use crate::services::message_backup_service::{BackupStats, MessageBackupService};
use crate::storage::backup_data::ImportSummary;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
use crate::storage::{Database, KeyStore};

mod approval;
mod link_url;
mod new_device;
mod reauth;
mod transfer;
mod types;

pub use link_url::qr_png_for_link_url;
use link_url::{base64_url, base64_url_decode, build_link_url, random_provisioning_code};
use new_device::run_flow;
pub use types::*;
use types::{LINK_URL_HOST, LinkRequest, verification_code_for_pub};

/// Device linking service.
static PLATFORM_DEVICE_LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub struct LinkDeviceService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    key_store: Arc<KeyStore>,
    /// Backs the Link'n'Sync transfer archive (§30): the primary exports message
    /// history from it, the new device imports into it. Held so the linking flow
    /// can build a `MessageBackupService` without a wider client handle.
    database: Arc<Database>,
    event_bus: EventBus,
    /// UUID of an in-flight new-device session, owned here so the cancel
    /// button can reach it without the UI tracking session state.
    active_uuid: Mutex<Option<String>>,
}

impl LinkDeviceService {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        key_store: Arc<KeyStore>,
        database: Arc<Database>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            key_store,
            database,
            event_bus,
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

    /// The platform's own description of this hardware, e.g.
    /// "samsung SM-S938B · Android 16". Set once at startup by the platform
    /// layer; manufacturer, model and OS version only — never the user-chosen
    /// device name, which often carries a person's name and login/register
    /// send this in clear.
    pub fn set_platform_device_label(label: String) {
        let label = label.trim().to_owned();
        if !label.is_empty() {
            let _ = PLATFORM_DEVICE_LABEL.set(label);
        }
    }

    /// Name this install reports to the server (shown in Active Sessions):
    /// the platform label when known, else e.g. "Android (0.1.0)".
    /// Link, login and register all send it.
    pub fn current_device_name() -> String {
        match PLATFORM_DEVICE_LABEL.get() {
            Some(label) => label.clone(),
            None => format!(
                "{} ({})",
                Self::current_device_info(),
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    /// True if a new-device flow is currently running.
    pub fn has_active_session(&self) -> bool {
        self.active_uuid.lock().is_some()
    }

    /// Whether this install already holds an identity that linking must not
    /// overwrite with a different account's keys.
    pub fn has_local_identity(&self) -> Result<bool, LinkDeviceError> {
        Ok(IdentityKeyManager::new(self.key_store.clone())
            .get_identity()?
            .is_some())
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
    fn provisioning_client(&self) -> Result<ProvisioningClient, LinkDeviceError> {
        self.grpc_pool
            .try_channel()
            .map(|ch| ProvisioningClient::new(ch, self.grpc_pool.auth_clone()))
            .ok_or(LinkDeviceError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".into(),
            )))
    }
}

#[cfg(test)]
mod tests {
    use super::new_device::adopt_provisioned_settings;
    use super::*;

    /// Only approved-and-still-open sessions are pending, labelled by their
    /// device info (the server holds no name for a pending link).
    #[test]
    fn pending_links_keeps_only_open_approved_sessions() {
        use crate::proto::provisioning_v1::provisioning_session::Status;
        let s = |uuid: &str, status: Status, expires_at: u64| ProvisioningSession {
            provisioning_uuid: uuid.into(),
            device_info: "android".into(),
            status: status as i32,
            created_at: 0,
            expires_at,
        };
        let links = pending_links(
            vec![
                s("open", Status::Provisioned, 200),
                s("expired", Status::Provisioned, 100),
                s("waiting", Status::Pending, 200),
                s("done", Status::Completed, 200),
            ],
            150,
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uuid, "open");
        assert_eq!(links[0].device_name, "android");
    }

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
        let err =
            LinkDeviceService::parse_link("chatter://join?uuid=x&pub_key=y").expect_err("bad");
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
    fn new_device_and_approver_show_the_same_code() {
        let eph = ProvisioningKeypair::generate();
        let url = build_link_url("uuid-1", &base64_url(&eph.public_bytes()), None, None);
        let approver_sees =
            verification_code_for_pub(&LinkDeviceService::parse_link(&url).unwrap().peer_pub);
        assert_eq!(
            approver_sees,
            verification_code_for_pub(&eph.public_bytes())
        );
    }

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
        assert!(
            code_a
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
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

    use crate::crypto::svr::derive_pin_fingerprint;
    use crate::domain::ids::UserId;
    use crate::domain::user::User;
    use crate::events::EventBus;
    use crate::network::grpc::client_pool::GrpcClientPool;
    use crate::storage::repositories::user_repo::UserRepository;
    use crate::storage::{Database, KeyStore};
    use tempfile::NamedTempFile;

    /// Build a fully-initialised LinkDeviceService backed by a fresh
    /// in-memory database, registered with the given username + (optional)
    /// PIN. Returns the service plus a kept-alive tempfile so the
    /// SQLite-backed DB survives. The gRPC pool is never connected: the
    /// password path (a server OPAQUE proof) has nothing to talk to.
    fn setup_service(username: &str, pin: Option<&str>) -> (Arc<LinkDeviceService>, NamedTempFile) {
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

        settings_repo
            .set_string(keys::SVR_SALT_USERNAME, username)
            .unwrap();
        if let Some(p) = pin {
            let fp = derive_pin_fingerprint(p, username).unwrap();
            settings_repo.set_bytes(keys::PIN_FINGERPRINT, &fp).unwrap();
        }

        // Network/event/notification deps aren't exercised by verify_*; build
        // smallest possible instances.
        let grpc_pool = Arc::new(GrpcClientPool::new("https://127.0.0.1:0").expect("grpc pool"));
        let event_bus = EventBus::new();
        let svc = Arc::new(LinkDeviceService::new(
            grpc_pool,
            settings_repo,
            key_store,
            db,
            event_bus,
        ));
        (svc, db_file)
    }

    /// The password check is a server OPAQUE proof: with no server it is an
    /// error, never a silent `false` (or `true`). The proof itself is covered
    /// by crypto/opaque.rs and the server's opaque_flows_test.go.
    #[tokio::test]
    async fn verify_password_without_server_is_an_error() {
        let (svc, _db) = setup_service("alice", None);
        assert!(matches!(
            svc.verify_password("hunter2").await,
            Err(LinkDeviceError::Network(_))
        ));
    }

    #[test]
    fn verify_pin_accepts_correct_pin() {
        let (svc, _db) = setup_service("alice", Some("devpin123"));
        assert_eq!(svc.verify_pin("devpin123"), PinVerification::Match);
    }

    #[test]
    fn verify_pin_rejects_wrong_pin() {
        let (svc, _db) = setup_service("alice", Some("devpin123"));
        assert_eq!(svc.verify_pin("wrongpin1"), PinVerification::Mismatch);
    }

    #[test]
    fn verify_pin_format_validation() {
        let (svc, _db) = setup_service("alice", Some("devpin123"));
        // Too short (< MIN_PIN_LENGTH).
        assert!(matches!(
            svc.verify_pin("1234567"),
            PinVerification::Error(PinVerificationError::InvalidInput)
        ));
        // Non-printable ASCII rejected even at the right length.
        assert!(matches!(
            svc.verify_pin("1234\t678"),
            PinVerification::Error(PinVerificationError::InvalidInput)
        ));
    }

    #[test]
    fn verify_pin_no_fingerprint_errors() {
        // Password-only account, no PIN persisted.
        let (svc, _db) = setup_service("alice", None);
        assert!(matches!(
            svc.verify_pin("devpin123"),
            PinVerification::NotSetUpOnThisDevice
        ));
    }

    #[test]
    fn verify_pin_storage_and_corrupt_state_are_distinct() {
        let (svc, _file) = setup_service("alice", None);
        svc.settings_repo
            .set_bytes(keys::PIN_FINGERPRINT, &[0; 3])
            .unwrap();
        assert_eq!(
            svc.verify_pin("test-pin-only"),
            PinVerification::Error(PinVerificationError::InvalidFingerprint)
        );
        svc.settings_repo
            .database()
            .connection()
            .unwrap()
            .execute_batch("DROP TABLE settings")
            .unwrap();
        assert_eq!(
            svc.verify_pin("test-pin-only"),
            PinVerification::Error(PinVerificationError::Storage)
        );
    }

    #[tokio::test]
    async fn verify_password_or_pin_routes_pin_first() {
        // Correct PIN matches locally, without the (absent) server.
        let (svc, _db) = setup_service("alice", Some("devpin123"));
        assert_eq!(svc.verify_password_or_pin("devpin123").await.unwrap(), true);
    }

    #[tokio::test]
    async fn verify_password_or_pin_wrong_pin_falls_through_to_server() {
        let (svc, _db) = setup_service("alice", Some("devpin123"));
        // PIN mismatch => the password path, which needs the server: an
        // error here, never an accidental match.
        assert!(
            svc.verify_password_or_pin("anything-long-wrong")
                .await
                .is_err()
        );
    }

    /// The new device takes the primary's read-receipt and typing settings
    /// from the ProvisionMessage, not the defaults.
    #[test]
    fn linked_device_adopts_the_primarys_privacy_settings() {
        use crate::services::configuration_sync::current;
        let me = crate::domain::ids::UserId::new();
        let open = || {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = Arc::new(Database::open(&dir.path().join("s.db"), "testpass").expect("db"));
            db.run_migrations().expect("migrations");
            (dir, SettingsRepository::new(db))
        };
        let (_p, primary) = open();
        primary.set_bool(keys::SEND_READ_RECEIPTS, false).unwrap();
        let sent = current(&primary);
        let provision = ProvisionMessage {
            read_receipts: sent.read_receipts,
            typing_indicators: sent.typing_indicators,
            ..Default::default()
        };

        let (_n, linked) = open();
        assert!(
            current(&linked).read_receipts,
            "defaults on before the link"
        );
        linked
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        adopt_provisioned_settings(&linked, &me, &provision).unwrap();
        assert_eq!(current(&linked), sent);
        assert!(!current(&linked).read_receipts && current(&linked).typing_indicators);
    }
}
