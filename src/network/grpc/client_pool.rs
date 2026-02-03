//! gRPC connection pooling.
//!
//! Connection pool management for gRPC channels with HTTP/2 keepalive.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tonic::transport::{Channel, Endpoint};

use crate::error::NetworkError;

use super::interceptors::AuthInterceptor;

/// gRPC connection pool with shared channel and authentication.
///
/// Manages a single tonic Channel that can be cloned cheaply for
/// multiple gRPC clients. Includes configuration for HTTP/2 keepalive
/// and connection timeouts.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// // Set authentication
/// pool.auth().set_bearer("my-jwt-token".to_string());
///
/// // Get channel for creating service clients
/// let channel = pool.channel();
/// ```
pub struct GrpcClientPool {
    channel: Arc<RwLock<Option<Channel>>>,
    endpoint: Endpoint,
    auth: AuthInterceptor,
}

impl GrpcClientPool {
    /// Create a new GrpcClientPool with the given server URL.
    ///
    /// Configures the endpoint with production-ready settings:
    /// - 10s connect timeout
    /// - 30s request timeout
    /// - 5m TCP keepalive
    /// - 30s HTTP/2 keepalive interval
    /// - 10s HTTP/2 keepalive timeout
    /// - HTTP/2 keepalive while idle enabled
    /// - 256 concurrent requests
    ///
    /// # Arguments
    ///
    /// * `server_url` - Server URL (e.g., "http://localhost:50051" or "https://api.example.com")
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the URL is invalid.
    pub fn new(server_url: &str) -> Result<Self, NetworkError> {
        let endpoint = Endpoint::from_shared(server_url.to_string())
            .map_err(|e| NetworkError::ConnectionFailed(format!("invalid URL: {}", e)))?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Some(Duration::from_secs(300)))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .concurrency_limit(256);

        Ok(Self {
            channel: Arc::new(RwLock::new(None)),
            endpoint,
            auth: AuthInterceptor::new(),
        })
    }

    /// Connect to the server.
    ///
    /// Establishes the underlying HTTP/2 connection. Must be called
    /// before using `channel()`.
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the connection fails.
    pub async fn connect(&self) -> Result<(), NetworkError> {
        let channel = self
            .endpoint
            .connect()
            .await
            .map_err(|e| NetworkError::ConnectionFailed(format!("connection failed: {}", e)))?;

        *self.channel.write() = Some(channel);
        Ok(())
    }

    /// Disconnect from the server.
    ///
    /// Drops the underlying channel. Safe to call multiple times.
    pub fn disconnect(&self) {
        *self.channel.write() = None;
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.channel.read().is_some()
    }

    /// Get the shared channel for creating service clients.
    ///
    /// # Panics
    ///
    /// Panics if not connected. Call `connect()` first.
    pub fn channel(&self) -> Channel {
        self.channel
            .read()
            .clone()
            .expect("GrpcClientPool: not connected - call connect() first")
    }

    /// Try to get the channel, returning None if not connected.
    pub fn try_channel(&self) -> Option<Channel> {
        self.channel.read().clone()
    }

    /// Get the authentication interceptor for setting credentials.
    ///
    /// Use this to configure authentication before making requests:
    ///
    /// ```ignore
    /// pool.auth().set_bearer("my-token".to_string());
    /// // or
    /// pool.auth().set_basic("user.1".to_string(), "password".to_string());
    /// ```
    pub fn auth(&self) -> &AuthInterceptor {
        &self.auth
    }

    /// Get a clone of the auth interceptor for use with service clients.
    ///
    /// Service clients need their own interceptor instance.
    pub fn auth_clone(&self) -> AuthInterceptor {
        self.auth.clone()
    }

    /// Get the configured server URL.
    pub fn server_url(&self) -> &str {
        self.endpoint.uri().host().unwrap_or("unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_valid_url() {
        let pool = GrpcClientPool::new("http://localhost:50051");
        assert!(pool.is_ok());
    }

    #[test]
    fn test_new_https_url() {
        let pool = GrpcClientPool::new("https://api.example.com:443");
        assert!(pool.is_ok());
    }

    #[test]
    fn test_new_invalid_url() {
        let pool = GrpcClientPool::new("not a valid url");
        assert!(pool.is_err());

        if let Err(NetworkError::ConnectionFailed(msg)) = pool {
            assert!(msg.contains("invalid URL"));
        } else {
            panic!("Expected ConnectionFailed error");
        }
    }

    #[test]
    fn test_initial_state() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();
        assert!(!pool.is_connected());
        assert!(pool.try_channel().is_none());
    }

    #[test]
    fn test_disconnect_when_not_connected() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();
        // Should not panic
        pool.disconnect();
        assert!(!pool.is_connected());
    }

    #[test]
    fn test_auth_interceptor_access() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();

        pool.auth().set_bearer("test-token".to_string());

        match pool.auth().current_mode() {
            super::super::interceptors::AuthMode::Bearer(token) => {
                assert_eq!(token, "test-token");
            }
            _ => panic!("Expected Bearer mode"),
        }
    }

    #[test]
    #[should_panic(expected = "not connected")]
    fn test_channel_panics_when_not_connected() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();
        let _ = pool.channel();
    }

    #[tokio::test]
    async fn test_connect_to_nonexistent_server() {
        let pool = GrpcClientPool::new("http://127.0.0.1:1").unwrap();
        let result = pool.connect().await;

        assert!(result.is_err());
        if let Err(NetworkError::ConnectionFailed(msg)) = result {
            assert!(msg.contains("connection failed"));
        } else {
            panic!("Expected ConnectionFailed error");
        }
    }
}
