//! gRPC interceptors.
//!
//! Authentication interceptor for injecting credentials into gRPC requests.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use parking_lot::RwLock;
use tonic::service::Interceptor;
use tonic::{Request, Status};

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
    /// No authentication.
    #[default]
    None,
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

    /// Clear authentication (set to None).
    pub fn clear(&self) {
        *self.auth.write() = AuthMode::None;
    }

    /// Get the current auth mode (for inspection/debugging).
    pub fn current_mode(&self) -> AuthMode {
        self.auth.read().clone()
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
    fn test_auth_interceptor_default() {
        let interceptor = AuthInterceptor::new();
        assert!(matches!(interceptor.current_mode(), AuthMode::None));
    }

    #[test]
    fn test_set_bearer() {
        let interceptor = AuthInterceptor::new();
        interceptor.set_bearer("my-device-token".to_string());

        match interceptor.current_mode() {
            AuthMode::Bearer(token) => assert_eq!(token, "my-device-token"),
            _ => panic!("Expected Bearer mode"),
        }
    }

    #[test]
    fn test_set_basic() {
        let interceptor = AuthInterceptor::new();
        interceptor.set_basic(
            "d04b0053-6557-48f9-86c2-e8ca389279bd.1".to_string(),
            "device-token".to_string(),
        );

        match interceptor.current_mode() {
            AuthMode::Basic { username, password } => {
                assert_eq!(username, "d04b0053-6557-48f9-86c2-e8ca389279bd.1");
                assert_eq!(password, "device-token");
            }
            _ => panic!("Expected Basic mode"),
        }
    }

    #[test]
    fn test_set_sealed_sender() {
        let interceptor = AuthInterceptor::new();
        let uak = [1u8; 16];
        interceptor.set_sealed_sender(uak);

        match interceptor.current_mode() {
            AuthMode::SealedSender { access_key } => assert_eq!(access_key, uak),
            _ => panic!("Expected SealedSender mode"),
        }
    }

    #[test]
    fn test_clear() {
        let interceptor = AuthInterceptor::new();
        interceptor.set_bearer("token".to_string());
        interceptor.clear();
        assert!(matches!(interceptor.current_mode(), AuthMode::None));
    }

    #[test]
    fn test_interceptor_bearer_header() {
        let mut interceptor = AuthInterceptor::new();
        interceptor.set_bearer("test-token".to_string());

        let request = Request::new(());
        let result = interceptor.call(request).unwrap();

        let auth = result.metadata().get("authorization").unwrap();
        assert_eq!(auth.to_str().unwrap(), "Bearer test-token");
    }

    #[test]
    fn test_interceptor_basic_header() {
        let mut interceptor = AuthInterceptor::new();
        interceptor.set_basic("user.1".to_string(), "pass".to_string());

        let request = Request::new(());
        let result = interceptor.call(request).unwrap();

        let auth = result.metadata().get("authorization").unwrap();
        // "user.1:pass" base64 encoded is "dXNlci4xOnBhc3M="
        assert_eq!(auth.to_str().unwrap(), "Basic dXNlci4xOnBhc3M=");
    }

    #[test]
    fn test_interceptor_sealed_sender_header() {
        let mut interceptor = AuthInterceptor::new();
        interceptor.set_sealed_sender([0xAB; 16]);

        let request = Request::new(());
        let result = interceptor.call(request).unwrap();

        let uak = result.metadata().get("unidentified-access-key").unwrap();
        // 16 bytes of 0xAB base64 encoded
        assert_eq!(uak.to_str().unwrap(), "q6urq6urq6urq6urq6urqw==");
    }

    #[test]
    fn test_interceptor_no_auth() {
        let mut interceptor = AuthInterceptor::new();

        let request = Request::new(());
        let result = interceptor.call(request).unwrap();

        assert!(result.metadata().get("authorization").is_none());
        assert!(result.metadata().get("unidentified-access-key").is_none());
    }
}
