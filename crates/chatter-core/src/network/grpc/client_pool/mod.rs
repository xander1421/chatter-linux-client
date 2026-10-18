//! gRPC connection pooling.
//!
//! Connection pool management for gRPC channels with HTTP/2 keepalive.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tonic::transport::Endpoint;

use crate::error::NetworkError;

use super::interceptors::AuthInterceptor;
use super::retry::{RetryConfig, retry_unary};
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

/// Tonic applies this budget to custom connectors too. Onion allows 120s
/// bootstrap + 90s dial + 10s scheduling slack; the connector enforces its own
/// 210s total cap. Direct TCP retains its existing 10s limit.
fn connect_timeout_for(server_url: &str) -> Duration {
    if server_url.contains(".onion") {
        Duration::from_secs(220)
    } else {
        Duration::from_secs(10)
    }
}

/// Ceiling on one `call_with_retry` sequence, retries and backoff included.
/// Preserved for direct and onion RPC responsiveness. A cold lazy onion RPC
/// may exhaust this 75s caller budget before bootstrap completes; callers that
/// need the full cold-start allowance must explicitly connect first. Endpoint
/// request limits (30s for non-streaming pools) and explicit caller timeouts
/// can be shorter still. They never extend the connector's 210s total cap.
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
/// let pool = GrpcClientPool::new("https://localhost:50051")?;
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

#[cfg(test)]
type ControlledDial =
    std::pin::Pin<Box<dyn Future<Output = Result<(GrpcTransport, bool), NetworkError>> + Send>>;

pub struct GrpcClientPool {
    #[cfg(test)]
    controlled_dial: parking_lot::Mutex<Option<ControlledDial>>,
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
    /// Bumped on every endpoint/transport swap. A `connect()` that started
    /// before a swap must not install its channel after it — that is how a
    /// direct dial still in flight when Shield was switched on would otherwise
    /// become the cached channel every later RPC rides.
    transport_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Separate channel for the unidentified (no auth header) clients of the
    /// ordinary sealed-sender DM path, so their traffic rides its OWN
    /// connection — an isolated Tor circuit under onion — and the server can't
    /// correlate those ops with authenticated RPCs by connection.
    unidentified_channel: Arc<RwLock<Option<GrpcTransport>>>,
    #[cfg(feature = "onion")]
    unidentified_health: Arc<RwLock<Option<Instant>>>,
    /// Channel for secret chats (anonymous DMs and groups). Always an isolated
    /// Tor circuit to the pinned `.onion`, whatever `transport_mode` is; never
    /// built over the direct transport. See [`Self::secret_channel`].
    secret_channel: Arc<RwLock<Option<GrpcTransport>>>,
    #[cfg(feature = "onion")]
    secret_health: Arc<RwLock<Option<Instant>>>,
    /// Cached QUIC reachability, probed once and reused. Only ever consulted
    /// for `TransportMode::Direct` over `https://` — onion carries TCP, and
    /// QUIC needs TLS 1.3 to negotiate ALPN.
    #[cfg(feature = "quic")]
    quic_probe: Arc<RwLock<QuicProbeState>>,
    /// Lazily-bootstrapped, pool-wide Tor client for the onion transport.
    /// Bootstrap is deferred to the first onion dial and cached thereafter.
    #[cfg(feature = "onion")]
    onion_tor: super::onion_transport::SharedTorClient,
    /// Cached ISOLATED Tor client for the unidentified channel (separate
    /// circuit-group from `onion_tor`). Derived from the bootstrapped client on
    /// first dial.
    #[cfg(feature = "onion")]
    unidentified_onion_tor: super::onion_transport::SharedTorClient,
    /// Cached ISOLATED Tor client for the secret-chat channel, its own
    /// circuit-group apart from both the account's and the unidentified one.
    #[cfg(feature = "onion")]
    secret_onion_tor: super::onion_transport::SharedTorClient,
}

mod anonymous_routes;
mod channels;
mod clients;
mod configuration;
mod connection;
mod retry;
mod secret_routes;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_flight_direct_probe_is_discarded_after_endpoint_generation_changes() {
        let pool = std::sync::Arc::new(GrpcClientPool::new("http://127.0.0.1:1").unwrap());
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        pool.controlled_dial.lock().replace(Box::pin(async move {
            accepted_tx.send(()).unwrap();
            let _ = release_rx.await;
            Err(NetworkError::ConnectionFailed(
                "controlled old dial failure".into(),
            ))
        }));
        let probing_pool = pool.clone();
        let probe = tokio::spawn(async move { probing_pool.probe().await });
        tokio::time::timeout(Duration::from_secs(1), accepted_rx)
            .await
            .unwrap()
            .unwrap();
        pool.set_url("http://127.0.0.1:1").unwrap();
        release_tx.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), probe)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transport changed while probing")
        );
    }

    #[test]
    fn test_new_valid_url() {
        let pool = GrpcClientPool::new("https://localhost:50051");
        assert!(pool.is_ok());
    }

    #[test]
    fn test_connect_timeout_onion_gets_long_deadline() {
        // Tonic's outer timeout must admit both full connector phases.
        assert_eq!(
            connect_timeout_for("http://abc123.onion:443"),
            Duration::from_secs(220)
        );
        // The scheme-less / bare-host forms both still route to the long path.
        assert_eq!(
            connect_timeout_for("nxpu2qh2eewzufrs.onion"),
            Duration::from_secs(220)
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

    #[cfg(feature = "onion")]
    #[test]
    fn onion_endpoint_admits_full_phase_budgets_but_unary_stays_bounded() {
        use super::super::onion_transport::{BOOTSTRAP_TIMEOUT, CONNECT_TIMEOUT, DIAL_TIMEOUT};
        assert_eq!(CONNECT_TIMEOUT, BOOTSTRAP_TIMEOUT + DIAL_TIMEOUT);
        assert!(connect_timeout_for("http://example.onion:443") > CONNECT_TIMEOUT);
        assert_eq!(UNARY_DEADLINE, Duration::from_secs(75));
        assert!(UNARY_DEADLINE < BOOTSTRAP_TIMEOUT);
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
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        assert!(!pool.is_connected());
        assert!(pool.try_channel().is_none());
    }

    #[test]
    fn test_disconnect_when_not_connected() {
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        // Should not panic
        pool.disconnect();
        assert!(!pool.is_connected());
    }

    #[test]
    fn test_transport_mode_switching() {
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
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

    /// Secret chats never fall back to the direct transport: with no `.onion`
    /// pinned, or without the `onion` feature, the secret channel and every
    /// secret client refuse with `SecretChatNeedsTor`.
    #[tokio::test]
    async fn secret_channel_fails_closed_without_onion() {
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        assert!(!pool.secret_route_available());
        assert!(matches!(
            pool.secret_channel(),
            Err(NetworkError::SecretChatNeedsTor(_))
        ));
        assert!(pool.secret_mailbox_client().is_err());
        assert!(pool.secret_message_client().is_err());
        assert!(pool.secret_auth_client().is_err());
        assert!(pool.secret_kt_client().is_err());
        assert!(pool.secret_file_client().is_err());

        pool.set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
        #[cfg(not(feature = "onion"))]
        {
            assert!(!pool.secret_route_available());
            let err = pool.secret_channel().err().expect("no Tor in this build");
            assert!(matches!(err, NetworkError::SecretChatNeedsTor(_)));
            assert!(err.to_string().contains("this build has none"), "{err}");
        }
        #[cfg(feature = "onion")]
        {
            // Built, and over Tor even though the pool itself stays Direct.
            assert!(pool.secret_route_available());
            assert!(pool.secret_channel().is_ok());
            assert_eq!(pool.transport_mode(), TransportMode::Direct);
            // Unpinning drops the cached channel: no stale route survives.
            pool.set_onion_address(None);
            assert!(pool.secret_channel().is_err());
        }
    }

    /// A direct-URL change while on onion (the sign-in form re-applies its
    /// Server URL field) must not put a clearnet endpoint under the onion
    /// dialer; it is remembered for the switch back.
    #[cfg(feature = "onion")]
    #[test]
    fn set_url_on_onion_keeps_the_onion_endpoint() {
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        pool.set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
        assert!(pool.onion_available());
        pool.set_transport_mode(TransportMode::Onion).unwrap();
        let generation = pool.snapshot().2;

        pool.set_url("https://127.0.0.1:50052").unwrap();
        assert_eq!(pool.transport_mode(), TransportMode::Onion);
        assert!(
            pool.endpoint().contains(".onion"),
            "endpoint: {}",
            pool.endpoint()
        );
        assert_eq!(pool.snapshot().2, generation, "no swap happened");

        pool.set_transport_mode(TransportMode::Direct).unwrap();
        assert!(
            pool.endpoint().contains("127.0.0.1:50052"),
            "endpoint: {}",
            pool.endpoint()
        );
    }

    /// The onion dialer refuses anything that is not a hidden service, so an
    /// endpoint/transport mismatch can never become a Tor-exit (or any other)
    /// connection to a clearnet host.
    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn onion_connector_refuses_clearnet_host() {
        use tower::Service;
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        let mut connector = pool.onion_connector();
        let err = connector
            .call("http://127.0.0.1:443".parse().unwrap())
            .await
            .err()
            .expect("clearnet host must be refused");
        assert!(err.to_string().contains("non-.onion"), "{err}");
    }

    #[tokio::test]
    async fn test_transport_switch_drops_cached_channel() {
        // The anonymity guarantee of anon mode depends on a transport switch
        // actually rebuilding the connection: if the switch left the old cached
        // channel in place, live traffic would keep riding the previous
        // transport (e.g. direct IP) after the user believed they were on Tor.
        // set_transport_mode must disconnect so the next channel() rebuilds.
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();

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
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();

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
        let pool = GrpcClientPool::new("https://localhost:50051").unwrap();
        // Should not panic — returns a lazy channel instead.
        let _ch = pool.channel();
        assert!(
            pool.is_connected(),
            "lazy channel should mark pool as connected"
        );
    }

    #[tokio::test]
    async fn test_connect_to_nonexistent_server() {
        let pool = GrpcClientPool::new("https://127.0.0.1:1").unwrap();
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
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            pool.dial(&endpoint, TransportMode::Direct),
        )
        .await;

        // dial() itself must never hang past its own QUIC_PROBE_TIMEOUT (3s)
        // plus a normal TCP connect attempt — the outer 10s is just a test
        // deadline, not the thing under test.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "dial() did not return promptly"
        );
        let result = result.expect("dial() must not hang");

        // TCP fallback also fails here (nothing listens on that port for TCP
        // either) — the point of this test is the QUIC decision, not a
        // successful connection.
        assert!(result.is_err());
        // TCP can fail before the independently spawned QUIC probe finishes.
        // Await its actual verdict; dial returning does not imply completion.
        tokio::time::timeout(QUIC_PROBE_TIMEOUT + Duration::from_secs(1), async {
            while *pool.quic_probe.read() == QuicProbeState::Probing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background QUIC probe must finish within its deadline");
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
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            pool.dial(&endpoint, TransportMode::Direct),
        )
        .await;

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
        let (_transport, fresh) = result.unwrap().unwrap();
        assert!(
            !fresh,
            "cached lazy QUIC construction must not claim wire contact"
        );
        assert_eq!(*pool.quic_probe.read(), QuicProbeState::Working);
    }

    #[cfg(feature = "quic")]
    #[tokio::test]
    async fn cached_quic_connect_and_probe_do_not_fabricate_authority() {
        use crate::network::connection::{
            ServerStatus, server_evidence_revision, server_status, set_server_status,
        };
        let pool = GrpcClientPool::new("https://192.0.2.1:50051").unwrap();
        *pool.quic_probe.write() = QuicProbeState::Working;
        for status in [
            ServerStatus::Unknown,
            ServerStatus::Unreachable,
            ServerStatus::Reachable,
        ] {
            set_server_status(status);
            let revision = server_evidence_revision();
            // Actual production cached-Working dial/build_quic, with no injected
            // completion and no remote peer: all construction is local.
            let connected = tokio::time::timeout(Duration::from_secs(1), pool.connect())
                .await
                .unwrap();
            assert_eq!(connected.is_ok(), status == ServerStatus::Reachable);
            assert!(
                pool.is_connected(),
                "cached transport should remain available for RPCs"
            );
            tokio::time::timeout(Duration::from_secs(1), pool.probe())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(server_status(), status);
            assert_eq!(server_evidence_revision(), revision);
        }
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
        for url in [
            "http://example.com",
            "http://192.0.2.1:50051",
            "http://example.onion",
            "https://user:secret@example.com",
            "ftp://example.com",
        ] {
            assert!(GrpcClientPool::validate_direct_url(url).is_err(), "{url}");
        }
        assert!(GrpcClientPool::validate_direct_url("https://example.com").is_ok());
    }
}
