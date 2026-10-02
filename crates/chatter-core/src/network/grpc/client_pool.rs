//! gRPC connection pooling.
//!
//! Connection pool management for gRPC channels with HTTP/2 keepalive.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tonic::transport::Endpoint;

use crate::error::NetworkError;

use super::interceptors::AuthInterceptor;
use super::retry::{retry_unary, RetryConfig};
use super::transport::GrpcTransport;

/// Ceiling on the QUIC reachability probe in [`GrpcClientPool::dial`]. QUIC/UDP
/// is blocked on some networks (hotel wifi, corporate firewalls); a hung
/// handshake must fall back to TCP quickly rather than eat into the caller's
/// own deadline (e.g. the 75s `UNARY_DEADLINE`).
#[cfg(feature = "quic")]
const QUIC_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Cached result of the QUIC reachability probe, process-lifetime (no
/// persistence — re-probing on every process start is cheap and self-heals if
/// a network's QUIC/UDP filtering changes; a TTL-based re-probe within one
/// run is unneeded until flaky-UDP users show up in the wild).
#[cfg(feature = "quic")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicProbeState {
    Untried,
    /// Probe running in the background; dials use TCP until it settles.
    Probing,
    Working,
    Failed,
}

/// Connect timeout for an endpoint URL.
///
/// Onion cold-start is slow: Arti bootstraps, then may retry a dead HS
/// descriptor/rendezvous circuit before one holds (~24s observed). A 10s
/// connect timeout kills it mid-retry. Direct TCP keeps the tight 10s.
fn connect_timeout_for(server_url: &str) -> Duration {
    if server_url.contains(".onion") {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(10)
    }
}

/// Ceiling on one `call_with_retry` sequence, retries and backoff included.
///
/// Generous enough that a slow server on a bad link still completes, short
/// enough that a user who tapped something while the server was gone gets an
/// answer rather than a spinner. It has to exceed the 60s onion connect timeout
/// or onion cold-start would be cut off mid-bootstrap.
const UNARY_DEADLINE: Duration = Duration::from_secs(75);

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
    channel: Arc<RwLock<Option<GrpcTransport>>>,
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
    anon_channel: Arc<RwLock<Option<GrpcTransport>>>,
    /// Cached QUIC reachability, probed once and reused. Only ever consulted
    /// for `TransportMode::Direct` over `https://` — onion carries TCP, and
    /// QUIC needs TLS 1.3 to negotiate ALPN.
    #[cfg(feature = "quic")]
    quic_probe: Arc<RwLock<QuicProbeState>>,
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
        Self::validate_direct_url(server_url)?;
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
            #[cfg(feature = "quic")]
            quic_probe: Arc::new(RwLock::new(QuicProbeState::Untried)),
            #[cfg(feature = "onion")]
            onion_tor: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "onion")]
            anon_onion_tor: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    /// Public transports must not send device credentials over cleartext HTTP.
    /// Onion endpoints are created separately and authenticated by Arti.
    fn validate_direct_url(server_url: &str) -> Result<(), NetworkError> {
        let url = url::Url::parse(server_url)
            .map_err(|_| NetworkError::ConnectionFailed("invalid URL".into()))?;
        let dev_loopback = cfg!(feature = "dev-trust-root") && match url.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if (url.scheme() != "https" && !(url.scheme() == "http" && dev_loopback))
            || !url.username().is_empty() || url.password().is_some() {
            return Err(NetworkError::ConnectionFailed("direct server connection requires HTTPS".into()));
        }
        Ok(())
    }

    /// Helper to create an endpoint with standard settings.
    fn create_endpoint(server_url: &str, timeout: Option<Duration>) -> Result<Endpoint, NetworkError> {
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
        Self::validate_direct_url(server_url)?;
        let new_endpoint = Self::create_endpoint(server_url, self.timeout)?;

        // Disconnect first
        self.disconnect();

        // Update endpoint
        *self.endpoint.write() = new_endpoint;
        *self.direct_url.write() = server_url.to_string();
        // quic_probe caches a verdict about the OLD server_url specifically
        // (reachable-over-QUIC or not) -- carrying it over to a genuinely
        // different destination is wrong in both directions: a stale
        // Working would skip probing a server that's never actually been
        // tried, and a stale Failed would give up on QUIC for a server that
        // might well support it. A new URL is a fair, cheap reason to
        // re-probe from scratch.
        #[cfg(feature = "quic")]
        {
            *self.quic_probe.write() = QuicProbeState::Untried;
        }

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

        let connect_result = self.dial(&endpoint).await;

        let channel = connect_result.map_err(|e| {
            crate::network::connection::set_server_status(
                crate::network::connection::ServerStatus::Unreachable,
            );
            NetworkError::ConnectionFailed(format!("connection failed: {}", e))
        })?;

        crate::network::connection::set_server_status(
            crate::network::connection::ServerStatus::Reachable,
        );
        *self.channel.write() = Some(channel);
        Ok(())
    }

    /// Dial the server, preferring QUIC when reachable.
    ///
    /// Only `TransportMode::Direct` over `https://` ever attempts QUIC — onion
    /// carries TCP (Tor has no UDP relay support) and plain `http://` has no
    /// TLS 1.3 to negotiate ALPN over. Reachability is probed once per pool
    /// (cached in `quic_probe`): a hit reuses the QUIC transport on every
    /// subsequent dial, a miss (or a timeout — some networks silently drop
    /// UDP) falls back to the existing TCP/HTTP2 path for the rest of the
    /// pool's life, matching today's behavior exactly when QUIC is off or
    /// unreachable.
    async fn dial(&self, endpoint: &Endpoint) -> Result<GrpcTransport, tonic::transport::Error> {
        // QUIC target for this dial, when its probe is still in flight.
        #[cfg(feature = "quic")]
        let mut racing_quic: Option<http::Uri> = None;

        #[cfg(feature = "quic")]
        {
            let direct = *self.transport_mode.read() == TransportMode::Direct;
            let probe_state = *self.quic_probe.read();
            let server_url = self.direct_url.read().clone();
            let uri = server_url
                .starts_with("https://")
                .then(|| server_url.parse::<http::Uri>().ok())
                .flatten();
            if let (true, Some(uri)) = (direct, uri) {
                match probe_state {
                    // Already proven reachable — build the channel, no probe.
                    // Re-probing on every dial() was a real bug: under
                    // reconnect churn it became a flood of UDP/TLS handshakes.
                    QuicProbeState::Working => match super::transport::build_quic(uri) {
                        Ok(transport) => return Ok(transport),
                        Err(e) => {
                            tracing::warn!(error = %e, "cached-working QUIC channel build failed — falling back to TCP");
                        }
                    },
                    // Probe in the background while TCP dials. It used to be
                    // awaited here, so a network that drops UDP (and the adb
                    // TCP tunnel) cost QUIC_PROBE_TIMEOUT — 3s of black screen
                    // — on every cold start before anything rendered.
                    QuicProbeState::Untried => {
                        *self.quic_probe.write() = QuicProbeState::Probing;
                        let state = self.quic_probe.clone();
                        let probe_uri = uri.clone();
                        tokio::spawn(async move {
                            let outcome = tokio::time::timeout(
                                QUIC_PROBE_TIMEOUT,
                                super::transport::probe_quic(&probe_uri),
                            )
                            .await;
                            let verdict = match outcome {
                                Ok(Ok(())) => QuicProbeState::Working,
                                Ok(Err(e)) => {
                                    tracing::info!(error = %e, "QUIC unreachable — staying on TCP");
                                    QuicProbeState::Failed
                                }
                                Err(_) => {
                                    tracing::info!("QUIC probe timed out — staying on TCP");
                                    QuicProbeState::Failed
                                }
                            };
                            // A server-URL change resets to Untried meanwhile;
                            // this verdict is about the old URL then, so drop it.
                            let mut current = state.write();
                            if *current == QuicProbeState::Probing {
                                *current = verdict;
                            }
                        });
                        racing_quic = Some(uri);
                    }
                    QuicProbeState::Probing | QuicProbeState::Failed => {}
                }
            }
        }

        // Onion transport tunnels the channel through Tor via the Arti connector;
        // Direct transport uses tonic's built-in TCP/TLS dialer.
        #[cfg(feature = "onion")]
        let channel = if *self.transport_mode.read() == TransportMode::Onion {
            endpoint.connect_with_connector(self.onion_connector()).await?
        } else {
            endpoint.connect().await?
        };
        #[cfg(not(feature = "onion"))]
        let channel = endpoint.connect().await?;

        // The probe won the race: prefer QUIC now rather than at the next dial.
        #[cfg(feature = "quic")]
        if let Some(uri) = racing_quic {
            if *self.quic_probe.read() == QuicProbeState::Working {
                if let Ok(transport) = super::transport::build_quic(uri) {
                    tracing::info!("transport: QUIC (probe succeeded)");
                    return Ok(transport);
                }
            }
        }

        Ok(channel.into())
    }

    /// Dial the server and throw the connection away: a reachability probe.
    ///
    /// Deliberately does **not** touch the pool's channel. [`connect`] does, and
    /// a monitor that re-dialled the live channel every half minute would tear
    /// down whatever server streams were riding it. Healing the pool is already
    /// somebody else's job: [`channel`] makes a lazy one on demand and
    /// [`call_with_retry`] drops stale ones.
    ///
    /// The only thing this is for is the answer, which it records in
    /// [`crate::network::connection::server_status`].
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the dial fails.
    ///
    /// [`connect`]: Self::connect
    /// [`channel`]: Self::channel
    /// [`call_with_retry`]: Self::call_with_retry
    pub async fn probe(&self) -> Result<(), NetworkError> {
        use crate::network::connection::{set_server_status, ServerStatus};

        let endpoint = self.endpoint.read().clone();
        let result = self.dial(&endpoint).await;

        match result {
            Ok(_channel) => {
                set_server_status(ServerStatus::Reachable);
                Ok(())
            }
            Err(e) => {
                set_server_status(ServerStatus::Unreachable);
                Err(NetworkError::ConnectionFailed(format!(
                    "connection failed: {e}"
                )))
            }
        }
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
    pub fn anon_channel(&self) -> GrpcTransport {
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
                let ch: GrpcTransport = endpoint
                    .connect_with_connector_lazy(self.anon_onion_connector())
                    .into();
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

    /// Demote a cached `Working` QUIC verdict to `Failed` after a real RPC
    /// over it has actually failed.
    ///
    /// `dial()` never re-probes once QUIC is cached `Working` — see the
    /// comment there — which means nothing ever demoted it either: probe
    /// succeeds on a network where UDP works, phone roams to one that
    /// blocks it, every subsequent RPC fails, and `dial()` keeps handing
    /// out a QUIC channel forever (`build_quic()` is pure local
    /// construction, no I/O, so it never itself errors). A transport
    /// failure on the channel actually in use is proof enough to stop
    /// trying QUIC for the rest of this pool's life — deliberately
    /// `Failed`, not `Untried`: `Untried` would re-probe on every
    /// reconnect and reintroduce the handshake-storm shape 96a77c45
    /// already fixed once. Only demotes from `Working`; leaves `Untried`
    /// (never got a chance yet) and `Failed` (already terminal) alone.
    #[cfg(feature = "quic")]
    fn note_transport_error(&self) {
        let mut probe = self.quic_probe.write();
        if *probe == QuicProbeState::Working {
            tracing::info!("QUIC channel failed in use — falling back to TCP for the rest of this session");
            *probe = QuicProbeState::Failed;
        }
    }
    #[cfg(not(feature = "quic"))]
    fn note_transport_error(&self) {}

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
    pub fn channel(&self) -> GrpcTransport {
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

        // A prior dial() already proved QUIC reachable — build a lazy QUIC
        // channel (no I/O until the first RPC, same cost profile as
        // connect_lazy()) instead of re-probing synchronously, which this
        // path must never do: callers rely on channel() never blocking.
        #[cfg(feature = "quic")]
        {
            let quic_ready = *self.transport_mode.read() == TransportMode::Direct
                && *self.quic_probe.read() == QuicProbeState::Working;
            if quic_ready {
                let server_url = self.direct_url.read().clone();
                if let Ok(uri) = server_url.parse::<http::Uri>() {
                    match super::transport::build_quic(uri) {
                        Ok(channel) => {
                            tracing::warn!("GrpcClientPool: no active connection — created lazy QUIC channel (RPCs will attempt on-demand connect)");
                            *guard = Some(channel.clone());
                            return channel;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "lazy QUIC channel build failed — falling back to TCP");
                        }
                    }
                }
            }
        }

        #[cfg(feature = "onion")]
        let channel: GrpcTransport = if *self.transport_mode.read() == TransportMode::Onion {
            endpoint.connect_with_connector_lazy(self.onion_connector()).into()
        } else {
            endpoint.connect_lazy().into()
        };
        #[cfg(not(feature = "onion"))]
        let channel: GrpcTransport = endpoint.connect_lazy().into();
        tracing::warn!("GrpcClientPool: no active connection — created lazy channel (RPCs will attempt on-demand connect)");
        *guard = Some(channel.clone());
        channel
    }

    /// Try to get the channel, returning None if not connected.
    pub fn try_channel(&self) -> Option<GrpcTransport> {
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
        use crate::network::connection::{set_server_status, ServerStatus};

        let pool = self;
        let retry_config = RetryConfig::default();
        let attempts = retry_unary(
            &retry_config,
            operation,
            Some(|| {
                tracing::warn!(
                    "Transport error detected — disconnecting stale channel for reconnect"
                );
                set_server_status(ServerStatus::Unreachable);
                pool.disconnect();
                pool.note_transport_error();
            }),
            || f(pool),
        );

        // The pool is deliberately built without a request deadline, because a
        // deadline on the endpoint would also cut off server streams mid-stream.
        // That left unary calls with nothing but the connect timeout, which only
        // fires for a server that refuses TCP — not for one that accepts the
        // connection and then never answers, where the call hung forever. The
        // deadline goes here, around the whole retry sequence rather than each
        // attempt, so the worst case a caller has to survive is one number.
        let result = match tokio::time::timeout(UNARY_DEADLINE, attempts).await {
            Ok(inner) => inner,
            Err(_) => {
                tracing::warn!(
                    operation,
                    deadline_s = UNARY_DEADLINE.as_secs(),
                    "RPC exceeded its deadline — treating the server as unreachable"
                );
                set_server_status(ServerStatus::Unreachable);
                pool.disconnect();
                pool.note_transport_error();
                Err(NetworkError::Timeout)
            }
        };

        // A completed RPC is the strongest evidence of reachability there is —
        // better than the synthetic dial the monitor falls back to — so record it
        // and spare the monitor a probe.
        if result.is_ok() {
            set_server_status(ServerStatus::Reachable);
        }
        result
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
        // In anon mode the attachment brokering must be anonymous too. The
        // FileService URL-mint methods are public (auth-exempt) and the object
        // key is an opaque random cdn_key, so the ONLY account linkage is the
        // Basic-auth header the interceptor would attach. Under onion transport
        // build the client with an EMPTY interceptor (AuthMode::None → no header)
        // over the ISOLATED anon circuit, so the server can't tie the cdn_key to
        // the account by credential or by connection. Combined with the CDN blob
        // transfer already routed over Tor (anon_http), this closes H3. Direct
        // transport keeps the authenticated client.
        #[cfg(feature = "onion")]
        if self.transport_mode() == TransportMode::Onion {
            return super::FileClient::new(self.anon_channel(), AuthInterceptor::new());
        }
        super::FileClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated AI broker client.
    pub fn ai_client(&self) -> super::ai_client::AiClient {
        super::ai_client::AiClient::new(self.channel(), self.auth_clone())
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

    /// Create an anonymous Key Transparency client (no auth headers).
    ///
    /// Fetches a peer's signed KT inclusion proof over the isolated anonymous
    /// channel so the anon-DM path can KT-gate session establishment without an
    /// authenticated identity (Phase 3a). `GetProof` is a public method
    /// server-side; the proof is public, auditable transparency data.
    pub fn anonymous_kt_client(&self) -> super::AnonymousKtClient {
        super::AnonymousKtClient::new(self.anon_channel())
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
    fn test_connect_timeout_onion_gets_long_deadline() {
        // Onion cold-start (~24s observed) must outlast the connect timeout,
        // otherwise the dial is killed mid HS-circuit retry.
        assert_eq!(
            connect_timeout_for("http://abc123.onion:443"),
            Duration::from_secs(60)
        );
        // The scheme-less / bare-host forms both still route to the long path.
        assert_eq!(
            connect_timeout_for("nxpu2qh2eewzufrs.onion"),
            Duration::from_secs(60)
        );
        // Direct TCP stays tight.
        assert_eq!(
            connect_timeout_for("http://localhost:50051"),
            Duration::from_secs(10)
        );
        assert_eq!(
            connect_timeout_for("https://api.example.com:443"),
            Duration::from_secs(10)
        );
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

    /// Bind a UDP socket that never completes a QUIC handshake — simulates a
    /// network that silently drops/filters QUIC (hotel wifi, some corporate
    /// firewalls) — and confirm `dial()` falls back to TCP rather than
    /// hanging past `QUIC_PROBE_TIMEOUT`, and that the failure is cached so
    /// later dials don't pay the probe cost again.
    #[cfg(feature = "quic")]
    #[tokio::test]
    async fn test_dial_falls_back_to_tcp_when_quic_unreachable() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = udp.local_addr().unwrap().port();
        let server_url = format!("https://127.0.0.1:{port}");

        let pool = GrpcClientPool::new(&server_url).unwrap();
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Untried);

        let endpoint = pool.endpoint.read().clone();
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(10), pool.dial(&endpoint)).await;

        // dial() itself must never hang past its own QUIC_PROBE_TIMEOUT (3s)
        // plus a normal TCP connect attempt — the outer 10s is just a test
        // deadline, not the thing under test.
        assert!(started.elapsed() < Duration::from_secs(10), "dial() did not return promptly");
        let result = result.expect("dial() must not hang");

        // TCP fallback also fails here (nothing listens on that port for TCP
        // either) — the point of this test is the QUIC decision, not a
        // successful connection.
        assert!(result.is_err());
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Failed);
    }

    /// Regression test for a real bug: `dial()` re-probed QUIC over the
    /// network on every call, even when `quic_probe` was already cached as
    /// `Working` — so any reconnect (a background stream erroring and
    /// retrying, several callers dialing around the same time) redid a full
    /// QUIC handshake attempt instead of reusing the known-good result. A
    /// cached `Working` state must skip straight to building a channel with
    /// no I/O — proven here by pointing at an address that would time out if
    /// actually probed, and asserting `dial()` still returns almost
    /// instantly.
    #[cfg(feature = "quic")]
    #[tokio::test]
    async fn test_dial_skips_network_probe_when_already_working() {
        // A non-routable address (RFC 5737 TEST-NET-1): a real probe against
        // it would hang until QUIC_PROBE_TIMEOUT (3s), not fail fast.
        let pool = GrpcClientPool::new("https://192.0.2.1:50051").unwrap();
        *pool.quic_probe.write() = QuicProbeState::Working;

        let endpoint = pool.endpoint.read().clone();
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(1), pool.dial(&endpoint)).await;

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "dial() took {elapsed:?} — should skip straight to build_quic() (no I/O) \
             when quic_probe is already Working, not re-probe the network"
        );
        // build_quic() is infallible I/O-wise (binds a local socket, does no
        // network round-trip), so this succeeds even though nothing is
        // listening at the far end — the real handshake is deferred to the
        // first RPC, same as connect_lazy().
        assert!(result.is_ok(), "dial() must not hang");
        assert!(result.unwrap().is_ok());
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Working);
    }

    /// Regression test for a real bug: nothing ever demoted a cached
    /// `Working` QUIC verdict, so once QUIC proved reachable it stayed
    /// "working" forever even after the network actually stopped
    /// supporting it (e.g. roaming onto a UDP-blocking network) — every
    /// subsequent dial() would keep handing out a QUIC channel that fails
    /// on first real use, since build_quic() does no I/O and can't itself
    /// detect the network changed.
    #[cfg(feature = "quic")]
    #[test]
    fn test_note_transport_error_demotes_working_to_failed() {
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        *pool.quic_probe.write() = QuicProbeState::Working;

        pool.note_transport_error();
        assert_eq!(
            *pool.quic_probe.read(),
            QuicProbeState::Failed,
            "a real transport failure must demote a cached Working verdict"
        );
    }

    /// note_transport_error() must be a no-op for Untried (hasn't had its
    /// probe chance yet — demoting it would skip straight to TCP without
    /// ever trying QUIC) and for Failed (already terminal).
    #[cfg(feature = "quic")]
    #[test]
    fn test_note_transport_error_leaves_untried_and_failed_alone() {
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Untried);
        pool.note_transport_error();
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Untried);

        *pool.quic_probe.write() = QuicProbeState::Failed;
        pool.note_transport_error();
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Failed);
    }

    /// Regression test: quic_probe caches a verdict about a specific
    /// server_url. set_url() must reset it, or a stale verdict from the
    /// old server (in either direction) silently carries over to a
    /// genuinely different one.
    #[cfg(feature = "quic")]
    #[test]
    fn test_set_url_resets_quic_probe() {
        let pool = GrpcClientPool::new("https://old-server:50051").unwrap();
        *pool.quic_probe.write() = QuicProbeState::Failed;

        pool.set_url("https://new-server:50051").unwrap();

        assert_eq!(
            *pool.quic_probe.read(),
            QuicProbeState::Untried,
            "a new server_url must get a fresh QUIC probe, not inherit the old one's verdict"
        );
    }
}

#[cfg(test)]
mod transport_security_tests {
    use super::GrpcClientPool;

    #[test]
    fn direct_remote_endpoints_require_tls() {
        for url in ["http://example.com", "http://192.0.2.1:50051", "http://example.onion", "https://user:secret@example.com", "ftp://example.com"] {
            assert!(GrpcClientPool::validate_direct_url(url).is_err(), "{url}");
        }
        assert!(GrpcClientPool::validate_direct_url("https://example.com").is_ok());
    }
}
