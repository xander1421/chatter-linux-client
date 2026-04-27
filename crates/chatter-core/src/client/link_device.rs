//! Device-linking façade.
//!
//! High-level façade methods over [`LinkDeviceService`] for both sides of
//! the QR-code device linking flow. UI code (chatter-ui) routes through
//! these methods, never the raw service — per the
//! `feedback_service_layer_architecture` memory.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::proto::provisioning_v1::ProvisioningSession;
use crate::services::link_device_service::{
    ApprovedDevice, LinkDeviceError, LinkDeviceService, LinkProgress,
};
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::user_repo::UserRepository;

use super::{ChatterClient, ClientError};

impl ChatterClient {
    /// Lazy accessor for the device-linking service.
    ///
    /// Constructed on first use because the deps (settings repo, user repo)
    /// are read-only and only touched when the user opens the link-device
    /// flow. Cheap to call repeatedly thereafter.
    pub(crate) fn link_device(&self) -> &Arc<LinkDeviceService> {
        self.link_device_service.get_or_init(|| {
            let settings = Arc::new(SettingsRepository::new(self.ui.database.clone()));
            let user_repo = Arc::new(UserRepository::new(self.ui.database.clone()));
            let key_store = self.signal().key_store.clone();
            Arc::new(LinkDeviceService::new(
                self.ui.grpc_pool.clone(),
                settings,
                user_repo,
                key_store,
                self.ui.event_bus.clone(),
                self.notifications().clone(),
            ))
        })
    }

    // ── New-device flow ────────────────────────────────────────────────

    /// Run the entire new-device QR flow as a single stream of progress
    /// events. The UI's only job is to render `QrReady`, update status on
    /// `WaitingForApproval` / `Approved`, and switch view + run post-auth
    /// bring-up on `Linked`. All session state, polling, retry, and error
    /// formatting stays in core.
    ///
    /// On `Linked`, callers must additionally call [`Self::start`] to
    /// kick off background services (this matches the contract of `login`
    /// / `register`). The returned `Linked` event already carries the
    /// final user/device IDs.
    ///
    /// Ensures the gRPC pool is connected before kicking off the flow —
    /// callers don't need to connect first.
    pub async fn link_device_run(
        self: &Arc<Self>,
    ) -> Result<mpsc::UnboundedReceiver<LinkProgress>, ClientError> {
        if !self.is_connected() {
            self.connect().await?;
        }
        let device_info = LinkDeviceService::current_device_info().to_string();
        let device_name = format!("{} ({})", device_info, env!("CARGO_PKG_VERSION"));
        Ok(self
            .link_device()
            .run_new_device_flow(device_info, device_name))
    }

    /// Cancel the in-flight new-device flow if any. UI calls this from the
    /// cancel button — no session ID needed.
    pub async fn link_device_cancel(&self) -> Result<(), LinkDeviceError> {
        self.link_device().cancel_active().await
    }

    // ── Primary-side flow ──────────────────────────────────────────────

    /// Single-shot primary-side approve. Parses the URL, verifies the
    /// password (constant-time against local profile key), then encrypts +
    /// uploads the provision envelope. Returns a summary for the UI to
    /// display ("Linked Alex's Laptop").
    ///
    /// Errors:
    /// - `InvalidLinkUrl(_)` — paste was malformed
    /// - `ReauthFailed` — wrong password
    /// - `ReauthUnsupportedForOidc` — OIDC user, no local password to check
    /// - other variants surface network / crypto failures
    pub async fn approve_link_from_url(
        &self,
        url: &str,
        password: &str,
    ) -> Result<ApprovedDevice, LinkDeviceError> {
        self.link_device().approve_link_from_url(url, password).await
    }

    /// Parse-only preview. Lets the approve modal show what device is
    /// about to be linked before the user types a password. Pure function,
    /// no network.
    pub fn preview_link_url(url: &str) -> Result<ApprovedDevice, LinkDeviceError> {
        LinkDeviceService::preview_link(url)
    }

    pub async fn list_pending_links(
        &self,
    ) -> Result<Vec<ProvisioningSession>, LinkDeviceError> {
        self.link_device().list_pending().await
    }
}
