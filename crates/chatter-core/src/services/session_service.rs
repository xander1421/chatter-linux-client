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
    /// Presentation copy for an authorization session that has ended.
    pub status_text: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last used.
    pub last_used: DateTime<Utc>,
    /// Whether this is the current session.
    pub is_current: bool,
    /// Phone or tablet (named "Android …" / "iOS …" by
    /// `LinkDeviceService::current_device_name`), for the row's icon.
    pub is_mobile: bool,
    /// Another device that may be signed out from here: not this one and not
    /// the primary (device 1), which the server refuses to revoke
    /// (`internal/services/auth/service/devices.go`).
    pub can_revoke: bool,
}

/// Phone or tablet, from the name `LinkDeviceService::current_device_name`
/// gives it ("Android (0.1.0)", "iOS (0.1.0)"). Picks the Active Sessions icon.
fn is_mobile_device_name(name: &str) -> bool {
    // "samsung SM-S938B · Android 16" carries the OS after the model.
    name.contains("Android") || name.contains("iOS")
}

/// Session management errors.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Operation requires user to be logged in.
    #[error("not logged in")]
    NotLoggedIn,

    #[error("session changed; reopen Active sessions")]
    SessionChanged,

    /// Some revocable devices could not be signed out. `failed` keeps each
    /// device's id and cause for logs; the message is user copy.
    #[error("{}", partial_revocation_message(*revoked, failed.len()))]
    PartialRevocation {
        revoked: usize,
        failed: Vec<(u32, String)>,
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

fn devices(n: usize) -> String {
    if n == 1 {
        "1 device".to_string()
    } else {
        format!("{n} devices")
    }
}

fn partial_revocation_message(revoked: usize, failed: usize) -> String {
    let head = if revoked == 0 {
        String::new()
    } else {
        format!("Signed out {}. ", devices(revoked))
    };
    format!(
        "{head}Could not sign out {}. Check your connection and try again.",
        devices(failed)
    )
}

/// The devices "Revoke all other sessions" would sign out: every listed
/// device this one may revoke (not itself, not the primary). The UI offers
/// the action only when this is non-zero.
pub fn revocable_count(sessions: &[Session]) -> usize {
    sessions.iter().filter(|s| s.can_revoke).count()
}

/// What "Revoke all other sessions" did when every revocable device was
/// signed out. Not an error: the primary staying signed in is the server's
/// rule (`RevokeDevice` refuses it), not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeAllOutcome {
    /// Devices signed out.
    pub revoked: usize,
    /// Another device is the primary and stayed signed in.
    pub primary_kept: bool,
}

impl RevokeAllOutcome {
    /// One line for the Active sessions screen.
    pub fn summary(&self) -> String {
        let head = if self.revoked == 0 {
            "No other devices to sign out.".to_string()
        } else {
            format!("Signed out {}.", devices(self.revoked))
        };
        if self.primary_kept {
            format!("{head} The primary device can't be signed out from another device.")
        } else {
            head
        }
    }
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
        self.settings_repo.current_user_id().ok()
    }

    /// List every linked device, including devices whose session has ended.
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
                    status_text: if s.has_active_session {
                        String::new()
                    } else {
                        "Signed out — sign in again on that device".to_string()
                    },
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
                    can_revoke: !s.is_current_session && s.device_id != 1,
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
    use super::{RevokeAllOutcome, SessionError, is_mobile_device_name};

    #[test]
    fn revoke_all_copy_is_human() {
        let o = RevokeAllOutcome {
            revoked: 2,
            primary_kept: true,
        };
        assert_eq!(
            o.summary(),
            "Signed out 2 devices. The primary device can't be signed out from another device."
        );
        let e = SessionError::PartialRevocation {
            revoked: 1,
            failed: vec![(3, "network".into())],
        };
        assert_eq!(
            e.to_string(),
            "Signed out 1 device. Could not sign out 1 device. Check your connection and try again."
        );
    }

    #[test]
    fn mobile_names_get_the_phone_icon() {
        assert!(is_mobile_device_name("Android (0.1.0)"));
        assert!(is_mobile_device_name("iOS (0.1.0)"));
        assert!(!is_mobile_device_name("Linux Desktop (0.1.0)"));
        assert!(!is_mobile_device_name("linux-client"));
    }
}
