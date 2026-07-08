//! gRPC connection pooling.
//!
//! Connection pool management for gRPC channels with HTTP/2 keepalive.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tonic::transport::{Channel, Endpoint};

use crate::error::NetworkError;

use super::interceptors::AuthInterceptor;
use super::retry::{retry_unary, RetryConfig};

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
/// Which network path the pool's connections take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// Direct TCP/TLS to the server URL.
    Direct,
    /// Over a Tor v3 onion service (requires the `onion` feature to actually
    /// route; without it, switching returns an error and the caller stays put).
    Onion,
}

pub struct GrpcClientPool {
    channel: Arc<RwLock<Option<Channel>>>,
    endpoint: Arc<RwLock<Endpoint>>,
    auth: AuthInterceptor,
    timeout: Option<Duration>,
    /// The direct server URL, remembered so we can rebuild a direct endpoint
    /// when switching transport back from onion.
    direct_url: Arc<RwLock<String>>,
    /// Pinned server .onion address (e.g. "http://<hash>.onion:443"), if any.
    onion_addr: Arc<RwLock<Option<String>>>,
    /// Active transport (Direct by default; anon mode flips it to Onion).
    transport_mode: Arc<RwLock<TransportMode>>,
    /// Separate channel for the anonymous mailbox/message/auth clients, so their
    /// traffic rides its OWN connection — an isolated Tor circuit under onion — and
    /// the server can't correlate anon ops with authenticated RPCs by connection.
    anon_channel: Arc<RwLock<Option<Channel>>>,
    /// Lazily-bootstrapped, pool-wide Tor client for the onion transport.
    /// Bootstrap is deferred to the first onion dial and cached thereafter.
    #[cfg(feature = "onion")]
    onion_tor: super::onion_transport::SharedTorClient,
    /// Cached ISOLATED Tor client for the anon channel (separate circuit-group
    /// from `onion_tor`). Derived from the bootstrapped client on first anon dial.
    #[cfg(feature = "onion")]
    anon_onion_tor: super::onion_transport::SharedTorClient,
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
            direct_url: Arc::new(RwLock::new(server_url.to_string())),
            onion_addr: Arc::new(RwLock::new(None)),
            transport_mode: Arc::new(RwLock::new(TransportMode::Direct)),
            anon_channel: Arc::new(RwLock::new(None)),
            #[cfg(feature = "onion")]
            onion_tor: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "onion")]
            anon_onion_tor: Arc::new(tokio::sync::OnceCell::new()),
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

        // Configure TLS for HTTPS endpoints (required on Android where system CAs
        // aren't automatically loaded by rustls)
        if server_url.starts_with("https://") {
            let tls_config = tonic::transport::ClientTlsConfig::new();
            endpoint = endpoint.tls_config(tls_config)
                .map_err(|e| NetworkError::ConnectionFailed(format!("TLS config error: {}", e)))?;
        }

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
        *self.direct_url.write() = server_url.to_string();

        tracing::info!("gRPC pool URL updated to: {}", server_url);
        Ok(())
    }

    /// Pin the server's Tor v3 onion address (e.g. "http://<hash>.onion:443").
    /// Required before switching to `TransportMode::Onion`.
    pub fn set_onion_address(&self, addr: Option<String>) {
        *self.onion_addr.write() = addr;
    }

    /// The currently active transport.
    pub fn transport_mode(&self) -> TransportMode {
        *self.transport_mode.read()
    }

    /// Shared handle to the pool's lazily-bootstrapped Tor client, for carrying
    /// direct HTTP (CDN content) over the same circuit substrate in anon mode.
    #[cfg(feature = "onion")]
    pub fn onion_tor(&self) -> super::onion_transport::SharedTorClient {
        self.onion_tor.clone()
    }

    /// Switch the network transport. `Direct` rebuilds a normal endpoint from the
    /// remembered server URL; `Onion` routes the whole pool over Tor to the pinned
    /// `.onion` (requires the `onion` feature). Rebuilding disconnects the current
    /// connection; the next `channel()` reconnects over the new transport.
    ///
    /// Returns an error (leaving the transport unchanged) if onion is requested
    /// but not available — callers treat that as "stay on the current path".
    pub fn set_transport_mode(&self, mode: TransportMode) -> Result<(), NetworkError> {
        match mode {
            TransportMode::Direct => {
                let url = self.direct_url.read().clone();
                let ep = Self::create_endpoint(&url, self.timeout)?;
                self.disconnect();
                *self.endpoint.write() = ep;
                *self.transport_mode.write() = TransportMode::Direct;
                tracing::info!("transport: direct ({url})");
                Ok(())
            }
            TransportMode::Onion => {
                let _onion = self.onion_addr.read().clone().ok_or_else(|| {
                    NetworkError::ConnectionFailed("no .onion address pinned".into())
                })?;
                #[cfg(feature = "onion")]
                {
                    // Build an h2c endpoint straight at the `.onion` (no TLS — the
                    // onion address self-authenticates). The real dial goes through
                    // the Arti connector on the next `channel()`/`connect()`.
                    // TODO(onion): isolate anon vs auth circuits — one shared circuit
                    // for the whole pool for now (hides client + server IP).
                    let ep = Self::create_endpoint(&_onion, self.timeout)?;
                    self.disconnect();
                    *self.endpoint.write() = ep;
                    *self.transport_mode.write() = TransportMode::Onion;
                    tracing::info!("transport: onion ({_onion})");
                    Ok(())
                }
                #[cfg(not(feature = "onion"))]
                {
                    Err(NetworkError::ConnectionFailed(
                        "onion transport not built (rebuild with --features onion)".into(),
                    ))
                }
            }
        }
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

        // Onion transport tunnels the channel through Tor via the Arti connector;
        // Direct transport uses tonic's built-in TCP/TLS dialer.
        #[cfg(feature = "onion")]
        let connect_result = if *self.transport_mode.read() == TransportMode::Onion {
            endpoint.connect_with_connector(self.onion_connector()).await
        } else {
            endpoint.connect().await
        };
        #[cfg(not(feature = "onion"))]
        let connect_result = endpoint.connect().await;

        let channel = connect_result
            .map_err(|e| NetworkError::ConnectionFailed(format!("connection failed: {}", e)))?;

        *self.channel.write() = Some(channel);
        Ok(())
    }

    /// Build a fresh onion connector sharing the pool's lazily-bootstrapped
    /// Tor client, so reconnects reuse the same warm circuit.
    #[cfg(feature = "onion")]
    fn onion_connector(&self) -> super::onion_transport::OnionConnector {
        super::onion_transport::OnionConnector::new(self.onion_tor.clone())
    }

    /// Isolated onion connector for the anonymous channel (separate circuit).
    #[cfg(feature = "onion")]
    fn anon_onion_connector(&self) -> super::onion_transport::OnionConnector {
        super::onion_transport::OnionConnector::new_isolated(
            self.onion_tor.clone(),
            self.anon_onion_tor.clone(),
        )
    }

    /// The channel for the anonymous mailbox/message/auth clients.
    ///
    /// Under onion transport this is a SEPARATE channel over an isolated Tor
    /// circuit, so the server cannot correlate anonymous mailbox ops with the
    /// account's authenticated RPCs by connection/circuit. On the direct
    /// transport isolation is moot (both connections share the client IP), so it
    /// falls back to the shared channel.
    pub fn anon_channel(&self) -> Channel {
        #[cfg(feature = "onion")]
        {
            if *self.transport_mode.read() == TransportMode::Onion {
                if let Some(ch) = self.anon_channel.read().clone() {
                    return ch;
                }
                let mut guard = self.anon_channel.write();
                if let Some(ch) = guard.clone() {
                    return ch;
                }
                let endpoint = self.endpoint.read().clone();
                let ch = endpoint.connect_with_connector_lazy(self.anon_onion_connector());
                *guard = Some(ch.clone());
                return ch;
            }
        }
        self.channel()
    }

    /// Disconnect from the server.
    ///
    /// Drops the underlying channel. Safe to call multiple times.
    pub fn disconnect(&self) {
        *self.channel.write() = None;
        *self.anon_channel.write() = None;
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.channel.read().is_some()
    }

    /// Get the shared channel for creating service clients.
    ///
    /// If the pool has not been connected yet (or a previous `connect()` failed),
    /// a **lazy channel** is created automatically. The lazy channel defers the
    /// real TCP handshake to the first gRPC call, so callers get a proper
    /// transport error instead of a panic when the server is unreachable.
    pub fn channel(&self) -> Channel {
        // Fast path: already connected.
        if let Some(ch) = self.channel.read().clone() {
            return ch;
        }

        // Slow path: create a lazy channel so callers never panic.
        let mut guard = self.channel.write();
        // Double-check after acquiring write lock.
        if let Some(ch) = guard.clone() {
            return ch;
        }
        let endpoint = self.endpoint.read().clone();
        #[cfg(feature = "onion")]
        let channel = if *self.transport_mode.read() == TransportMode::Onion {
            endpoint.connect_with_connector_lazy(self.onion_connector())
        } else {
            endpoint.connect_lazy()
        };
        #[cfg(not(feature = "onion"))]
        let channel = endpoint.connect_lazy();
        tracing::warn!("GrpcClientPool: no active connection — created lazy channel (RPCs will attempt on-demand connect)");
        *guard = Some(channel.clone());
        channel
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
    // Retry middleware — transport-aware retry for all RPCs
    // ========================================================================

    /// Execute an RPC with automatic retry on transport errors.
    ///
    /// On transport failures the pool disconnects the stale channel so the
    /// next attempt creates a fresh HTTP/2 connection via the lazy-channel
    /// path. This fixes the "stale connection pool" problem where HTTP/2
    /// connections silently die and subsequent RPCs fail with
    /// `Code::Unknown / transport error`.
    ///
    /// The closure receives `&Self` so it can create a fresh client per
    /// attempt (important — a client holding a stale channel must not be
    /// reused after disconnect).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let events = pool.call_with_retry("CreateChannel", |p| {
    ///     let mut client = p.channel_client();
    ///     async move { client.create_channel("test", ChannelType::ChannelPublic, None, vec![], None, None, None).await }
    /// }).await?;
    /// ```
    pub async fn call_with_retry<F, Fut, T>(
        &self,
        operation: &str,
        mut f: F,
    ) -> Result<T, NetworkError>
    where
        F: FnMut(&Self) -> Fut,
        Fut: Future<Output = Result<T, NetworkError>>,
    {
        let pool = self;
        retry_unary(
            &RetryConfig::default(),
            operation,
            Some(|| {
                tracing::warn!(
                    "Transport error detected — disconnecting stale channel for reconnect"
                );
                pool.disconnect();
            }),
            || f(pool),
        )
        .await
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

    /// Create an authenticated SvrClient for backup operations.
    pub fn svr_client(&self) -> super::SvrClient {
        super::SvrClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated NotificationClient.
    pub fn notification_client(&self) -> super::NotificationClient {
        super::NotificationClient::new(self.channel(), self.auth_clone())
    }

    /// Create an anonymous mailbox client (no auth headers).
    pub fn anonymous_mailbox_client(&self) -> super::AnonymousMailboxClient {
        super::AnonymousMailboxClient::new(self.anon_channel())
    }

    /// Create an anonymous message client (no auth headers).
    pub fn anonymous_message_client(&self) -> super::AnonymousMessageClient {
        super::AnonymousMessageClient::new(self.anon_channel())
    }

    /// Create an anonymous auth client (no auth headers).
    pub fn anonymous_auth_client(&self) -> super::AnonymousAuthClient {
        super::AnonymousAuthClient::new(self.anon_channel())
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
    fn test_transport_mode_switching() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();
        assert_eq!(pool.transport_mode(), TransportMode::Direct);

        // Direct rebuild always works (no connect, just an endpoint swap).
        assert!(pool.set_transport_mode(TransportMode::Direct).is_ok());

        // Onion with no pinned .onion fails, and must leave the mode unchanged.
        assert!(pool.set_transport_mode(TransportMode::Onion).is_err());
        assert_eq!(pool.transport_mode(), TransportMode::Direct);

        // With an address pinned, behaviour splits on the `onion` feature:
        pool.set_onion_address(Some("http://abc.onion:443".into()));
        #[cfg(not(feature = "onion"))]
        {
            // Feature off: still "not built" — fail closed, mode unchanged
            // (no false anonymity).
            assert!(pool.set_transport_mode(TransportMode::Onion).is_err());
            assert_eq!(pool.transport_mode(), TransportMode::Direct);
        }
        #[cfg(feature = "onion")]
        {
            // Feature on: the onion endpoint builds and the mode flips (the
            // actual Tor dial is deferred to the next channel()).
            assert!(pool.set_transport_mode(TransportMode::Onion).is_ok());
            assert_eq!(pool.transport_mode(), TransportMode::Onion);
        }
    }

    #[tokio::test]
    async fn test_transport_switch_drops_cached_channel() {
        // The anonymity guarantee of anon mode depends on a transport switch
        // actually rebuilding the connection: if the switch left the old cached
        // channel in place, live traffic would keep riding the previous
        // transport (e.g. direct IP) after the user believed they were on Tor.
        // set_transport_mode must disconnect so the next channel() rebuilds.
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();

        // Materialise a (lazy) channel — pool now reports connected + caches it.
        let _ch = pool.channel();
        assert!(pool.is_connected(), "channel() should cache a channel");

        // Any transport switch — even Direct→Direct — must drop the cache.
        pool.set_transport_mode(TransportMode::Direct).unwrap();
        assert!(
            !pool.is_connected(),
            "transport switch must drop the cached channel so the next \
             channel() rebuilds over the new transport"
        );
        assert!(pool.try_channel().is_none());
    }

    #[test]
    fn test_auth_interceptor_access() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();

        pool.auth().set_bearer("test-token".to_string());

        match &pool.auth().current_mode() {
            super::super::interceptors::AuthMode::Bearer(token) => {
                assert_eq!(token, "test-token");
            }
            _ => panic!("Expected Bearer mode"),
        }
    }

    #[tokio::test]
    async fn test_channel_returns_lazy_when_not_connected() {
        let pool = GrpcClientPool::new("http://localhost:50051").unwrap();
        // Should not panic — returns a lazy channel instead.
        let _ch = pool.channel();
        assert!(pool.is_connected(), "lazy channel should mark pool as connected");
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
