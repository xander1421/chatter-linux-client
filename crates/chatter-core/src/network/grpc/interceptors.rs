//! gRPC interceptors.
//!
//! Authentication interceptor for injecting credentials into gRPC requests.

use std::fmt;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use parking_lot::RwLock;
use tonic::service::Interceptor;
use tonic::{Request, Status};
use zeroize::Zeroize;

use crate::mailbox::blind_signature::UnblindedToken;

/// Authentication modes for gRPC requests.
///
/// Secret material in `Bearer` / `Basic.password` / `SealedSender.access_key`
/// is zeroized when the variant is dropped (including when the active mode
/// is replaced via `set_*`). `Debug` is intentionally not derived so the
/// enum cannot leak credentials through accidental `{:?}` formatting.
#[derive(Clone, Default)]
pub enum AuthMode {
    /// Bearer token (device token).
    Bearer(String),
    /// Basic auth (Signal-style: {uuid}.{device_id}:token).
    Basic {
        /// Username (format: {uuid}.{device_id})
        username: String,
        /// Password (device auth token)
        password: String,
    },
    /// Sealed sender with unidentified access key.
    SealedSender {
        /// 16-byte UAK for anonymous sending
        access_key: [u8; 16],
    },
    /// Anonymous credential (spent blind-signed token).
    /// Used for initial contact or anonymous proxy services (e.g. GIF search).
    Anonymous(UnblindedToken),
    /// No authentication.
    #[default]
    None,
}

impl Drop for AuthMode {
    fn drop(&mut self) {
        match self {
            AuthMode::Bearer(token) => token.zeroize(),
            AuthMode::Basic { password, .. } => password.zeroize(),
            AuthMode::SealedSender { access_key } => access_key.zeroize(),
            // Anonymous tokens are blind-signed credentials; their crate owns
            // their lifecycle. No secret material in `None`.
            AuthMode::Anonymous(_) | AuthMode::None => {}
        }
    }
}


/// Thread-safe authentication interceptor for gRPC requests.
///
/// Injects appropriate authorization headers based on the current auth mode.
/// Designed to be cloned and shared across multiple gRPC clients.
#[derive(Clone)]
pub struct AuthInterceptor {
    auth: Arc<RwLock<AuthMode>>,
}

impl AuthInterceptor {
    /// Create a new AuthInterceptor with no authentication.
    pub fn new() -> Self {
        Self {
            auth: Arc::new(RwLock::new(AuthMode::None)),
        }
    }

    /// Set bearer token authentication (device token).
    pub fn set_bearer(&self, token: String) {
        *self.auth.write() = AuthMode::Bearer(token);
    }

    /// Set basic authentication (Signal-style device auth).
    ///
    /// # Arguments
    ///
    /// * `username` - Format: `{uuid}.{device_id}` (e.g., "d04b0053-6557-48f9-86c2-e8ca389279bd.1")
    /// * `password` - Device authentication token
    pub fn set_basic(&self, username: String, password: String) {
        *self.auth.write() = AuthMode::Basic { username, password };
    }

    /// Set sealed sender authentication with unidentified access key.
    ///
    /// Used for anonymous message sending where the server cannot
    /// identify the sender.
    pub fn set_sealed_sender(&self, access_key: [u8; 16]) {
        *self.auth.write() = AuthMode::SealedSender { access_key };
    }

    /// Set anonymous authentication with a blind-signed token.
    ///
    /// Used for initial contact or anonymous services like GIF search.
    pub fn set_anonymous(&self, token: UnblindedToken) {
        *self.auth.write() = AuthMode::Anonymous(token);
    }

    /// Set the authentication mode directly.
    pub fn set_mode(&self, mode: AuthMode) {
        *self.auth.write() = mode;
    }

    /// Clear authentication (set to None).
    pub fn clear(&self) {
        *self.auth.write() = AuthMode::None;
    }

    /// Get the current auth mode (for inspection/debugging).
    pub fn current_mode(&self) -> AuthMode {
        self.auth.read().clone()
    }

    /// Get the current user ID from Basic auth (format: {uuid}.{device_id}).
    /// Returns None if not in Basic auth mode or if parsing fails.
    pub fn get_user_id(&self) -> Option<String> {
        let auth = self.auth.read();
        match &*auth {
            AuthMode::Basic { username, .. } => {
                // Username format: {uuid}.{device_id}
                username.split('.').next().map(|s| s.to_string())
            }
            _ => None,
        }
    }

    /// Get full auth info (user_id, device_id, token) from Basic auth.
    ///
    /// Returns None if not in Basic auth mode or if parsing fails.
    pub fn get_auth_info(&self) -> Option<AuthInfo> {
        let auth = self.auth.read();
        match &*auth {
            AuthMode::Basic { username, password } => {
                // Username format: {uuid}.{device_id}
                let parts: Vec<&str> = username.splitn(2, '.').collect();
                if parts.len() != 2 {
                    return None;
                }
                let device_id: u32 = parts[1].parse().ok()?;
                Some(AuthInfo {
                    user_id: parts[0].to_string(),
                    device_id,
                    token: password.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Authentication credentials extracted from AuthInterceptor.
///
/// `token` is the device auth token (HTTP Basic password). `Debug` is
/// redacted and the token is zeroized on drop so handing this struct to a
/// log statement or letting it go out of scope cannot leak the secret.
#[derive(Clone)]
pub struct AuthInfo {
    /// User UUID
    pub user_id: String,
    /// Device ID
    pub device_id: u32,
    /// Auth token
    pub token: String,
}

impl fmt::Debug for AuthInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthInfo")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Drop for AuthInfo {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl Default for AuthInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let auth = self.auth.read();

        match &*auth {
            AuthMode::Bearer(token) => {
                let value = format!("Bearer {}", token);
                request.metadata_mut().insert(
                    "authorization",
                    value.parse().map_err(|_| {
                        Status::internal("invalid authorization header value")
                    })?,
                );
            }
            AuthMode::Basic { username, password } => {
                let credentials = format!("{}:{}", username, password);
                let encoded = BASE64.encode(credentials);
                let value = format!("Basic {}", encoded);
                request.metadata_mut().insert(
                    "authorization",
                    value.parse().map_err(|_| {
                        Status::internal("invalid authorization header value")
                    })?,
                );
            }
            AuthMode::SealedSender { access_key } => {
                let encoded = BASE64.encode(access_key);
                request.metadata_mut().insert(
                    "unidentified-access-key",
                    encoded.parse().map_err(|_| {
                        Status::internal("invalid UAK header value")
                    })?,
                );
            }
            AuthMode::Anonymous(token) => {
                // Serialize token to JSON for the header
                // Match the backend's AnonymousCredential struct
                let cred = serde_json::json!({
                    "public_key": BASE64.encode(&token.public_key),
                    "signature": BASE64.encode(&token.signature),
                });
                let value = cred.to_string();
                request.metadata_mut().insert(
                    "x-anonymous-credential",
                    value.parse().map_err(|_| {
                        Status::internal("invalid anonymous credential header value")
                    })?,
                );
            }
            AuthMode::None => {
                // No authentication header
            }
        }

        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_info_debug_redacts_token() {
        let info = AuthInfo {
            user_id: "alice".into(),
            device_id: 1,
            token: "super-secret-token".into(),
        };
        let dbg = format!("{:?}", info);
        assert!(dbg.contains("REDACTED"), "debug output: {dbg}");
        assert!(dbg.contains("alice"), "user_id should still be visible: {dbg}");
        assert!(!dbg.contains("super-secret-token"), "token leaked: {dbg}");
    }

    #[test]
    fn replacing_basic_auth_zeroizes_old_password() {
        // Smoke test: setting a new mode drops the old one (which runs the
        // zeroize Drop). We can't observe the freed memory, but we can at
        // least confirm the API contract holds without panicking.
        let interceptor = AuthInterceptor::new();
        interceptor.set_basic("alice.1".into(), "first-password".into());
        interceptor.set_basic("alice.1".into(), "second-password".into());
        interceptor.clear();
    }
}
