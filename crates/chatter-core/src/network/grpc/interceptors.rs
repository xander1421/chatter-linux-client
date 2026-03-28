//! gRPC interceptors.
//!
//! Authentication interceptor for injecting credentials into gRPC requests.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use parking_lot::RwLock;
use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::mailbox::blind_signature::UnblindedToken;

/// Authentication modes for gRPC requests.
#[derive(Clone)]
#[derive(Default)]
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


/// Thread-safe authentication interceptor for gRPC requests.
///
/// Injects appropriate authorization headers based on the current auth mode.
/// Also injects `X-Org-Id` header when an organization context is active.
/// Designed to be cloned and shared across multiple gRPC clients.
#[derive(Clone)]
pub struct AuthInterceptor {
    auth: Arc<RwLock<AuthMode>>,
    org_id: Arc<RwLock<Option<String>>>,
}

impl AuthInterceptor {
    /// Create a new AuthInterceptor with no authentication.
    pub fn new() -> Self {
        Self {
            auth: Arc::new(RwLock::new(AuthMode::None)),
            org_id: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the active organization ID for the `X-Org-Id` header.
    pub fn set_org_id(&self, org_id: String) {
        *self.org_id.write() = Some(org_id);
    }

    /// Clear the active organization ID.
    pub fn clear_org_id(&self) {
        *self.org_id.write() = None;
    }

    /// Get the current organization ID, if set.
    pub fn current_org_id(&self) -> Option<String> {
        self.org_id.read().clone()
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
#[derive(Debug, Clone)]
pub struct AuthInfo {
    /// User UUID
    pub user_id: String,
    /// Device ID
    pub device_id: u32,
    /// Auth token
    pub token: String,
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

        // Inject X-Org-Id header when an organization context is active
        let org_id = self.org_id.read();
        if let Some(ref id) = *org_id {
            request.metadata_mut().insert(
                "x-org-id",
                id.parse().map_err(|_| {
                    Status::internal("invalid org-id header value")
                })?,
            );
        }

        Ok(request)
    }
}
