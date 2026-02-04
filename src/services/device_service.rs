//! Device management service.
//!
//! List, rename, and revoke user devices (Signal-style multi-device support).

use chrono::{DateTime, Utc};
use std::sync::Arc;

use crate::error::{NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Device information.
#[derive(Debug, Clone)]
pub struct Device {
    /// Signal device ID (1 for primary, 2-127 for linked devices).
    pub device_id: u32,
    /// User-assigned device name.
    pub name: String,
    /// Device description (e.g., "Linux Desktop", "iPhone 15").
    pub info: String,
    /// When the device was first registered.
    pub created_at: DateTime<Utc>,
    /// When the device was last active.
    pub last_active: DateTime<Utc>,
    /// Whether this is the primary device (device_id = 1).
    pub is_primary: bool,
    /// Whether this is the current device making the request.
    pub is_current: bool,
}

/// Device management errors.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    /// Operation requires user to be logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// Cannot revoke the primary device.
    #[error("cannot revoke primary device")]
    CannotRevokePrimary,

    /// Cannot revoke the current device (use logout instead).
    #[error("cannot revoke current device")]
    CannotRevokeCurrent,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Device management service.
///
/// Provides device listing, renaming, and revocation functionality.
pub struct DeviceService {
    settings_repo: Arc<SettingsRepository>,
}

impl DeviceService {
    /// Create a new DeviceService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self { settings_repo }
    }

    /// Check if a user is currently logged in.
    fn is_logged_in(&self) -> bool {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .is_some()
    }

    /// Get the current device ID if logged in.
    fn current_device_id(&self) -> Option<u32> {
        self.settings_repo.get(keys::CURRENT_DEVICE_ID).ok().flatten()
    }

    /// List all devices for the current user.
    ///
    /// Returns devices sorted by device_id (primary first).
    pub async fn list_devices(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<Vec<Device>, DeviceError> {
        if !self.is_logged_in() {
            return Err(DeviceError::NotLoggedIn);
        }

        let response = auth_client.list_devices().await?;

        let mut devices: Vec<Device> = response
            .devices
            .into_iter()
            .map(|d| Device {
                device_id: d.device_id,
                name: d.device_name,
                info: d.device_info,
                created_at: DateTime::from_timestamp_millis(d.created_at as i64)
                    .unwrap_or(DateTime::UNIX_EPOCH),
                last_active: DateTime::from_timestamp_millis(d.last_active as i64)
                    .unwrap_or(DateTime::UNIX_EPOCH),
                is_primary: d.is_primary,
                is_current: d.is_current,
            })
            .collect();

        // Sort by device_id (primary device first)
        devices.sort_by_key(|d| d.device_id);

        Ok(devices)
    }

    /// Rename a device.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Signal device ID to rename
    /// * `new_name` - New name for the device
    pub async fn rename_device(
        &self,
        auth_client: &mut AuthClient,
        device_id: u32,
        new_name: &str,
    ) -> Result<(), DeviceError> {
        if !self.is_logged_in() {
            return Err(DeviceError::NotLoggedIn);
        }

        auth_client.rename_device(device_id, new_name).await?;
        Ok(())
    }

    /// Revoke a device (unlink from account).
    ///
    /// This will invalidate the device's credentials and remove it from the account.
    /// Cannot revoke the primary device (device_id = 1) or the current device.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Signal device ID to revoke
    pub async fn revoke_device(
        &self,
        auth_client: &mut AuthClient,
        device_id: u32,
    ) -> Result<(), DeviceError> {
        if !self.is_logged_in() {
            return Err(DeviceError::NotLoggedIn);
        }

        // Cannot revoke primary device
        if device_id == 1 {
            return Err(DeviceError::CannotRevokePrimary);
        }

        // Cannot revoke current device (use logout instead)
        if Some(device_id) == self.current_device_id() {
            return Err(DeviceError::CannotRevokeCurrent);
        }

        auth_client.revoke_device(device_id).await?;
        Ok(())
    }
}
