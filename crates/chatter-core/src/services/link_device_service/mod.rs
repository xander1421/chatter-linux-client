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

use link_url::{
    base64_url, base64_url_decode, build_link_url, qr_png_for_link_url, random_provisioning_code,
};
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
mod tests;
