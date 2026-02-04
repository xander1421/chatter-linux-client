//! Session management service.
//!
//! List sessions, revoke individual or all sessions.

use chrono::{DateTime, Utc};
use std::sync::Arc;

use crate::error::{NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Session information for a device.
#[derive(Debug, Clone)]
pub struct Session {
    /// Signal device ID.
    pub device_id: u32,
    /// Device description (e.g., "Linux Desktop", "iPhone 15").
    pub device_info: String,
    /// IP address of the last connection.
    pub ip_address: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last used.
    pub last_used: DateTime<Utc>,
    /// Whether this is the current session.
    pub is_current: bool,
}

/// Session management errors.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Operation requires user to be logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Session management service.
///
/// Provides session listing and revocation functionality.
pub struct SessionService {
    settings_repo: Arc<SettingsRepository>,
}

impl SessionService {
    /// Create a new SessionService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self { settings_repo }
    }

    /// Check if a user is currently logged in.
    pub fn is_logged_in(&self) -> bool {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .is_some()
    }

    /// Get the current device ID if logged in.
    pub fn current_device_id(&self) -> Option<u32> {
        self.settings_repo.get(keys::CURRENT_DEVICE_ID).ok().flatten()
    }

    /// List all active sessions for the current user.
    ///
    /// Returns a list of sessions with device info, timestamps, and whether
    /// each session is the current one (as provided by the server).
    pub async fn list_sessions(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<Vec<Session>, SessionError> {
        if !self.is_logged_in() {
            return Err(SessionError::NotLoggedIn);
        }

        let response = auth_client.list_sessions().await?;

        let sessions = response
            .sessions
            .into_iter()
            .map(|s| Session {
                device_id: s.device_id,
                device_info: s.device_info,
                ip_address: s.ip_address,
                created_at: DateTime::from_timestamp_millis(s.created_at as i64)
                    .unwrap_or_else(|| DateTime::UNIX_EPOCH),
                last_used: DateTime::from_timestamp_millis(s.last_used as i64)
                    .unwrap_or_else(|| DateTime::UNIX_EPOCH),
                is_current: s.is_current_session,
            })
            .collect();

        Ok(sessions)
    }

    /// Revoke a specific session by device ID.
    ///
    /// This will invalidate the session and force the device to re-authenticate.
    /// Cannot revoke the current session - use logout instead.
    pub async fn revoke_session(
        &self,
        auth_client: &mut AuthClient,
        device_id: u32,
    ) -> Result<(), SessionError> {
        if !self.is_logged_in() {
            return Err(SessionError::NotLoggedIn);
        }

        auth_client.revoke_session(device_id).await?;
        Ok(())
    }

    /// Revoke all sessions except the current one.
    ///
    /// This will invalidate all other sessions, forcing those devices to re-authenticate.
    /// The server automatically excludes the current session.
    pub async fn revoke_all_other_sessions(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<(), SessionError> {
        if !self.is_logged_in() {
            return Err(SessionError::NotLoggedIn);
        }

        auth_client.revoke_all_sessions().await?;
        Ok(())
    }
}
