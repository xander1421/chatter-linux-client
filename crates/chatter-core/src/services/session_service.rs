//! Session management service.
//!
//! List sessions, revoke individual or all sessions.

use chrono::{DateTime, Utc};
use std::sync::Arc;

use crate::error::{NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::device_service::readable_device_name;
use crate::storage::key_store::KeyStore;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

/// Session information for a device.
#[derive(Debug, Clone)]
pub struct Session {
    /// Signal device ID.
    pub device_id: u32,
    /// The row's label: the device's name (decrypted with the account
    /// identity key), else its description (e.g. "Android (0.1.0)").
    pub device_info: String,
    /// IP address of the last connection.
    pub ip_address: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last used.
    pub last_used: DateTime<Utc>,
    /// Whether this is the current session.
    pub is_current: bool,
    /// Phone or tablet (named "Android …" / "iOS …" by
    /// `LinkDeviceService::current_device_name`), for the row's icon.
    pub is_mobile: bool,
}

/// Phone or tablet, from the name `LinkDeviceService::current_device_name`
/// gives it ("Android (0.1.0)", "iOS (0.1.0)"). Picks the Active Sessions icon.
fn is_mobile_device_name(name: &str) -> bool {
    name.starts_with("Android") || name.starts_with("iOS")
}

/// Session management errors.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Operation requires user to be logged in.
    #[error("not logged in")]
    NotLoggedIn,

    #[error("session changed; reopen Active sessions")]
    SessionChanged,

    #[error(
        "revoked {revoked} session(s); failed devices: {failed:?}; protected primary devices: {protected:?}"
    )]
    PartialRevocation {
        revoked: usize,
        failed: Vec<(u32, String)>,
        protected: Vec<u32>,
    },

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Device revoke/rotate failed.
    #[error("device error: {0}")]
    Device(#[from] crate::services::device_service::DeviceError),
}

/// Session management service.
///
/// Provides session listing and revocation functionality.
pub struct SessionService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    /// Holds the account identity key the device names are encrypted to.
    key_store: Arc<KeyStore>,
}

impl SessionService {
    /// Create a new SessionService with dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        key_store: Arc<KeyStore>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            key_store,
        }
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
        self.settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
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

        let mut sessions: Vec<Session> = response
            .sessions
            .into_iter()
            .map(|s| {
                // The platform string: this device's own from what this build
                // IS (an older session's stored info may say "linux-client" on
                // a phone), every other device's from the server.
                let platform = if s.is_current_session {
                    crate::services::link_device_service::LinkDeviceService::current_device_name()
                } else {
                    s.device_info
                };
                // The server cannot read names (they are encrypted to our
                // identity key), so the name-else-info choice it used to make
                // is made here.
                let name = readable_device_name(&self.key_store, &s.encrypted_name);
                Session {
                    device_id: s.device_id,
                    is_mobile: is_mobile_device_name(&platform),
                    device_info: name.unwrap_or(platform),
                    ip_address: s.ip_address,
                    // NANOSECONDS, not millis. auth.proto:371-372 says so
                    // ("Unix timestamp in nanoseconds") and the server sends
                    // UnixNano() (active_sessions.go:51-52). Parsing nanos as millis
                    // puts the value far outside chrono's range, so
                    // from_timestamp_millis returned None and the unwrap_or below
                    // turned every session's times into 1970-01-01 — a plausible
                    // date, which is why it read as "no timestamps" rather than as a
                    // parse failure (OPEN-ITEMS §112d).
                    created_at: DateTime::from_timestamp_nanos(s.created_at as i64),
                    last_used: DateTime::from_timestamp_nanos(s.last_used as i64),
                    is_current: s.is_current_session,
                }
            })
            .collect();
        // This device first: it is the one row the user must never mistake
        // for another (REVIEW-R4 P0-4).
        sessions.sort_by_key(|s| !s.is_current);

        Ok(sessions)
    }

    // ==================== Settings ====================

    /// Save a boolean setting.
    pub fn set_bool_setting(&self, key: &str, value: bool) -> Result<(), SessionError> {
        self.settings_repo
            .set(key, &value)
            .map_err(SessionError::from)
    }

    /// Get a boolean setting.
    pub fn get_bool_setting(&self, key: &str) -> Result<Option<bool>, SessionError> {
        self.settings_repo.get(key).map_err(SessionError::from)
    }

    /// Save a string setting.
    pub fn set_string_setting(&self, key: &str, value: &str) -> Result<(), SessionError> {
        self.settings_repo
            .set_string(key, value)
            .map_err(SessionError::from)
    }

    /// Get a string setting.
    pub fn get_string_setting(&self, key: &str) -> Result<Option<String>, SessionError> {
        self.settings_repo
            .get_string(key)
            .map_err(SessionError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::is_mobile_device_name;

    #[test]
    fn mobile_names_get_the_phone_icon() {
        assert!(is_mobile_device_name("Android (0.1.0)"));
        assert!(is_mobile_device_name("iOS (0.1.0)"));
        assert!(!is_mobile_device_name("Linux Desktop (0.1.0)"));
        assert!(!is_mobile_device_name("linux-client"));
    }
}
