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
/// pool.auth().set_bearer("my-device-token".to_string());
///
/// // Get channel for creating service clients
/// let channel = pool.channel();
/// ```
pub struct GrpcClientPool {
    channel: Arc<RwLock<Option<Channel>>>,
    endpoint: Arc<RwLock<Endpoint>>,
    auth: AuthInterceptor,
    timeout: Option<Duration>,
}

impl GrpcClientPool {
    /// Create a new GrpcClientPool with the given server URL.
    ///
    /// Configures the endpoint with production-ready settings:
    /// - 10s connect timeout
    /// - 30s request timeout (use `new_for_streaming` for long-lived streams)
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
        Self::with_timeout(server_url, Some(Duration::from_secs(30)))
    }

    /// Create a new GrpcClientPool optimized for streaming endpoints.
    ///
    /// Like `new()` but without request timeout, suitable for long-lived
    /// server-side streaming connections.
    ///
    /// # Arguments
    ///
    /// * `server_url` - Server URL (e.g., "http://localhost:50051")
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the URL is invalid.
    pub fn new_for_streaming(server_url: &str) -> Result<Self, NetworkError> {
        Self::with_timeout(server_url, None)
    }

    /// Create a new GrpcClientPool with custom timeout.
    ///
    /// # Arguments
    ///
    /// * `server_url` - Server URL
    /// * `timeout` - Request timeout (None for no timeout, suitable for streaming)
    fn with_timeout(server_url: &str, timeout: Option<Duration>) -> Result<Self, NetworkError> {
        let endpoint = Self::create_endpoint(server_url, timeout)?;

        Ok(Self {
            channel: Arc::new(RwLock::new(None)),
            endpoint: Arc::new(RwLock::new(endpoint)),
            auth: AuthInterceptor::new(),
            timeout,
        })
    }

    /// Helper to create an endpoint with standard settings.
    fn create_endpoint(server_url: &str, timeout: Option<Duration>) -> Result<Endpoint, NetworkError> {
        let mut endpoint = Endpoint::from_shared(server_url.to_string())
            .map_err(|e| NetworkError::ConnectionFailed(format!("invalid URL: {}", e)))?
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Some(Duration::from_secs(300)))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .concurrency_limit(256)
            .initial_stream_window_size(2 * 1024 * 1024)
            .initial_connection_window_size(8 * 1024 * 1024);

        if let Some(t) = timeout {
            endpoint = endpoint.timeout(t);
        }

        Ok(endpoint)
    }

    /// Update the server URL.
    ///
    /// Disconnects any existing connection and updates the endpoint.
    /// Call `connect()` again after this to re-establish the connection.
    ///
    /// # Arguments
    ///
    /// * `server_url` - New server URL
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the URL is invalid.
    pub fn set_url(&self, server_url: &str) -> Result<(), NetworkError> {
        let new_endpoint = Self::create_endpoint(server_url, self.timeout)?;
        
        // Disconnect first
        self.disconnect();
        
        // Update endpoint
        *self.endpoint.write() = new_endpoint;
        
        tracing::info!("gRPC pool URL updated to: {}", server_url);
        Ok(())
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
        let endpoint = self.endpoint.read().clone();
        
        let channel = endpoint
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
    pub fn server_url(&self) -> String {
        self.endpoint.read().uri().host().unwrap_or("unknown").to_string()
    }

    /// Get the full endpoint URL string.
    ///
    /// Returns the complete URL including scheme and port.
    pub fn endpoint(&self) -> String {
        self.endpoint.read().uri().to_string()
    }

    // ========================================================================
    // Client factories — eliminate repeated `Client::new(channel(), auth_clone())`
    // ========================================================================

    /// Create an authenticated AuthClient.
    pub fn auth_client(&self) -> super::AuthClient {
        super::AuthClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated MessageClient.
    pub fn message_client(&self) -> super::MessageClient {
        super::MessageClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated MailboxClient.
    pub fn mailbox_client(&self) -> super::MailboxClient {
        super::MailboxClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated FileClient.
    pub fn file_client(&self) -> super::FileClient {
        super::FileClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated ChannelClient.
    pub fn channel_client(&self) -> super::ChannelClient {
        super::ChannelClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated CallClient.
    pub fn call_client(&self) -> super::CallClient {
        super::CallClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated UserClient.
    pub fn user_client(&self) -> super::UserClient {
        super::UserClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated GifClient.
    pub fn gif_client(&self) -> super::GifClient {
        super::GifClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated IntegrationClient.
    pub fn integration_client(&self) -> super::IntegrationClient {
        super::IntegrationClient::new(self.channel(), self.auth_clone())
    }

    /// Create an anonymous mailbox client (no auth headers).
    pub fn anonymous_mailbox_client(&self) -> super::AnonymousMailboxClient {
        super::AnonymousMailboxClient::new(self.channel())
    }

    /// Create an anonymous message client (no auth headers).
    pub fn anonymous_message_client(&self) -> super::AnonymousMessageClient {
        super::AnonymousMessageClient::new(self.channel())
    }

    /// Create an anonymous auth client (no auth headers).
    pub fn anonymous_auth_client(&self) -> super::AnonymousAuthClient {
        super::AnonymousAuthClient::new(self.channel())
    }

    /// Get auth info (user_id, device_id, token) from the auth interceptor.
    ///
    /// # Panics
    ///
    /// Panics if not in Basic auth mode or credentials are malformed.
    pub fn auth_info(&self) -> super::interceptors::AuthInfo {
        self.auth.get_auth_info()
            .expect("GrpcClientPool: auth_info requires Basic auth mode")
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
