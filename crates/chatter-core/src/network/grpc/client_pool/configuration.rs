use super::*;

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
    pub(super) fn with_timeout(
        server_url: &str,
        timeout: Option<Duration>,
    ) -> Result<Self, NetworkError> {
        Self::validate_direct_url(server_url)?;
        let endpoint = Self::create_endpoint(server_url, timeout)?;

        Ok(Self {
            #[cfg(test)]
            controlled_dial: parking_lot::Mutex::new(None),
            channel: Arc::new(RwLock::new(None)),
            endpoint: Arc::new(RwLock::new(endpoint)),
            auth: AuthInterceptor::new(),
            timeout,
            direct_url: Arc::new(RwLock::new(server_url.to_string())),
            onion_addr: Arc::new(RwLock::new(None)),
            transport_mode: Arc::new(RwLock::new(TransportMode::Direct)),
            transport_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            unidentified_channel: Arc::new(RwLock::new(None)),
            #[cfg(feature = "onion")]
            unidentified_health: Arc::new(RwLock::new(None)),
            secret_channel: Arc::new(RwLock::new(None)),
            #[cfg(feature = "onion")]
            secret_health: Arc::new(RwLock::new(None)),
            #[cfg(feature = "quic")]
            quic_probe: Arc::new(RwLock::new(QuicProbeState::Untried)),
            #[cfg(feature = "quic")]
            quic_probe_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(feature = "onion")]
            onion_tor: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "onion")]
            unidentified_onion_tor: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "onion")]
            secret_onion_tor: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    /// Public transports must not send device credentials over cleartext HTTP.
    /// Onion endpoints are created separately and authenticated by Arti.
    pub(super) fn validate_direct_url(server_url: &str) -> Result<(), NetworkError> {
        let url = url::Url::parse(server_url)
            .map_err(|_| NetworkError::ConnectionFailed("invalid URL".into()))?;
        let dev_loopback = cfg!(feature = "dev-trust-root")
            && match url.host() {
                Some(url::Host::Domain("localhost")) => true,
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                _ => false,
            };
        if (url.scheme() != "https" && !(url.scheme() == "http" && dev_loopback))
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(NetworkError::ConnectionFailed(
                "direct server connection requires HTTPS".into(),
            ));
        }
        Ok(())
    }

    /// Helper to create an endpoint with standard settings.
    pub(super) fn create_endpoint(
        server_url: &str,
        timeout: Option<Duration>,
    ) -> Result<Endpoint, NetworkError> {
        let mut endpoint = Endpoint::from_shared(server_url.to_string())
            .map_err(|e| NetworkError::ConnectionFailed(format!("invalid URL: {}", e)))?
            .connect_timeout(connect_timeout_for(server_url))
            .tcp_keepalive(Some(Duration::from_secs(300)))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .concurrency_limit(256)
            .initial_stream_window_size(2 * 1024 * 1024)
            .initial_connection_window_size(8 * 1024 * 1024);

        // TLS for HTTPS endpoints — Signal-style private-PKI pin (§8): trust ONLY
        // the provisioned chatter CA, never the system/native roots, so a rogue
        // or mis-issued public CA cannot MITM this channel. `ClientTlsConfig::new()`
        // starts with an EMPTY trust store in tonic 0.12; we add just our CA and
        // never call `with_native_roots()`. Onion endpoints are `http://` over Tor,
        // pinned by their `.onion` address, and never reach this branch.
        if endpoint.uri().scheme_str() == Some("https") {
            let ca_pem = crate::config::TLS_ROOT_CA_PEM.ok_or_else(|| {
                NetworkError::ConnectionFailed(
                    "no TLS root CA provisioned; refusing to open an unpinned TLS connection \
                     (build with CHATTER_TLS_ROOT_CA_PEM, or --features dev-trust-root)"
                        .to_string(),
                )
            })?;
            let tls_config = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(tonic::transport::Certificate::from_pem(ca_pem));
            endpoint = endpoint
                .tls_config(tls_config)
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
        Self::validate_direct_url(server_url)?;
        let new_endpoint = Self::create_endpoint(server_url, self.timeout)?;

        #[cfg(feature = "quic")]
        {
            let mut probe = self.quic_probe.write();
            *self.direct_url.write() = server_url.to_string();
            self.quic_probe_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            *probe = QuicProbeState::Untried;
        }
        #[cfg(not(feature = "quic"))]
        {
            *self.direct_url.write() = server_url.to_string();
        }
        if self.transport_mode() == TransportMode::Onion {
            // On onion the direct URL is only remembered for the switch back.
            // Installing it as the endpoint would leave an onion-mode pool
            // pointed at a clearnet host.
            tracing::info!("gRPC pool direct URL updated to {server_url} (stays on onion)");
            return Ok(());
        }
        self.swap_endpoint(new_endpoint, TransportMode::Direct);
        tracing::info!("gRPC pool URL updated to: {}", server_url);
        Ok(())
    }

    /// Pin the server's Tor v3 onion address (e.g. "http://<hash>.onion:443").
    /// Required before switching to `TransportMode::Onion`, and for any
    /// secret-chat traffic. A changed address drops the cached secret channel,
    /// which was built from the old one.
    pub fn set_onion_address(&self, addr: Option<String>) {
        let mut secret = self.secret_channel.write();
        let mut current = self.onion_addr.write();
        if *current != addr {
            *secret = None;
            #[cfg(feature = "onion")]
            {
                *self.secret_health.write() = None;
            }
        }
        *current = addr;
    }

    /// The currently active transport.
    pub fn transport_mode(&self) -> TransportMode {
        *self.transport_mode.read()
    }

    /// Shared handle to the pool's lazily-bootstrapped Tor client, for carrying
    /// direct HTTP (CDN content) over the same circuit substrate in anon mode.
    #[cfg(feature = "onion")]
    pub fn onion_tor(&self) -> super::super::onion_transport::SharedTorClient {
        self.onion_tor.clone()
    }

    /// Whether this build can route over Tor at all: the `onion` feature is
    /// compiled in AND a server `.onion` is pinned. Shield's network half is
    /// only offered when this holds.
    pub fn onion_available(&self) -> bool {
        cfg!(feature = "onion") && self.onion_addr.read().is_some()
    }

    /// Install a new endpoint + transport and drop every cached channel, as
    /// one step. Holding the channel write lock across the swap is what makes
    /// it atomic for `channel()`, which builds its lazy channel under that same
    /// lock: before this, `disconnect()` ran first, and a `channel()` landing
    /// between it and the endpoint write cached a channel on the OLD transport
    /// (direct) that then outlived the switch to onion.
    pub(super) fn swap_endpoint(&self, endpoint: Endpoint, mode: TransportMode) {
        let mut channel = self.channel.write();
        *self.endpoint.write() = endpoint;
        *self.transport_mode.write() = mode;
        self.transport_gen
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *channel = None;
        *self.unidentified_channel.write() = None;
        #[cfg(feature = "onion")]
        {
            *self.unidentified_health.write() = None;
        }
    }

    /// Endpoint, transport and generation read together (under the channel
    /// lock `swap_endpoint` holds), so a dial never pairs a new endpoint with
    /// the old transport's dialer.
    pub(super) fn snapshot(&self) -> (Endpoint, TransportMode, u64) {
        let _guard = self.channel.read();
        (
            self.endpoint.read().clone(),
            *self.transport_mode.read(),
            self.transport_gen
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    /// Record the outcome of a real onion dial in the Tor indicator, so the
    /// UI can say "connecting over Tor" / "connected" before any login-only
    /// loop (the inbox poller) exists to do it.
    pub(super) fn note_onion_dial(mode: TransportMode, ok: bool) {
        if mode == TransportMode::Onion {
            use crate::network::connection::{TorStatus, set_tor_status};
            set_tor_status(if ok {
                TorStatus::Connected
            } else {
                TorStatus::Error
            });
        }
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
                self.swap_endpoint(ep, TransportMode::Direct);
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
                    let ep = Self::create_endpoint(&_onion, self.timeout)?;
                    self.swap_endpoint(ep, TransportMode::Onion);
                    tracing::info!("transport: onion");
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
}
