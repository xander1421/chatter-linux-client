//! Session management service.
//!
//! List sessions, revoke individual or all sessions.

use chrono::{DateTime, Utc};
use std::sync::Arc;

use crate::error::{NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
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
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
}

impl SessionService {
    /// Create a new SessionService with dependencies.
    pub fn new(grpc_pool: Arc<GrpcClientPool>, settings_repo: Arc<SettingsRepository>) -> Self {
        Self { grpc_pool, settings_repo }
    }

    /// Create an auth client for network operations.
    fn auth_client(&self) -> AuthClient {
        self.grpc_pool.auth_client()
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

    /// Get the current user ID if logged in.
    pub fn current_user_id(&self) -> Option<crate::domain::ids::UserId> {
        use crate::services::current_user::CurrentUserProvider;
        self.settings_repo.current_user_id().ok()
    }

    /// List all active sessions for the current user.
    ///
    /// Returns a list of sessions with device info, timestamps, and whether
    /// each session is the current one (as provided by the server).
    pub async fn list_sessions(&self) -> Result<Vec<Session>, SessionError> {
        if !self.is_logged_in() {
            return Err(SessionError::NotLoggedIn);
        }

        let mut auth_client = self.auth_client();
        let response = auth_client.list_sessions().await?;

        let sessions = response
            .sessions
            .into_iter()
            .map(|s| Session {
                device_id: s.device_id,
                device_info: s.device_info,
                ip_address: s.ip_address,
                created_at: DateTime::from_timestamp_millis(s.created_at as i64)
                    .unwrap_or(DateTime::UNIX_EPOCH),
                last_used: DateTime::from_timestamp_millis(s.last_used as i64)
                    .unwrap_or(DateTime::UNIX_EPOCH),
                is_current: s.is_current_session,
            })
            .collect();

        Ok(sessions)
    }

    /// Revoke a specific session by device ID.
    ///
    /// This will invalidate the session and force the device to re-authenticate.
    /// Cannot revoke the current session - use logout instead.
    pub async fn revoke_session(&self, device_id: u32) -> Result<(), SessionError> {
        if !self.is_logged_in() {
            return Err(SessionError::NotLoggedIn);
        }

        let mut auth_client = self.auth_client();
        auth_client.revoke_session(device_id).await?;
        Ok(())
    }

    /// Revoke all sessions except the current one.
    ///
    /// This will invalidate all other sessions, forcing those devices to re-authenticate.
    /// The server automatically excludes the current session.
    pub async fn revoke_all_other_sessions(&self) -> Result<(), SessionError> {
        if !self.is_logged_in() {
            return Err(SessionError::NotLoggedIn);
        }

        let mut auth_client = self.auth_client();
        auth_client.revoke_all_sessions().await?;
        Ok(())
    }

    // ==================== Settings ====================

    /// Save a boolean setting.
    pub fn set_bool_setting(&self, key: &str, value: bool) -> Result<(), SessionError> {
        self.settings_repo.set(key, &value).map_err(SessionError::from)
    }

    /// Get a boolean setting.
    pub fn get_bool_setting(&self, key: &str) -> Result<Option<bool>, SessionError> {
        self.settings_repo.get(key).map_err(SessionError::from)
    }

    /// Save a string setting.
    pub fn set_string_setting(&self, key: &str, value: &str) -> Result<(), SessionError> {
        self.settings_repo.set_string(key, value).map_err(SessionError::from)
    }

    /// Get a string setting.
    pub fn get_string_setting(&self, key: &str) -> Result<Option<String>, SessionError> {
        self.settings_repo.get_string(key).map_err(SessionError::from)
    }
}
