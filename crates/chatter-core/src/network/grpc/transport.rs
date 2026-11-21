//! `GrpcTransport`: TCP/HTTP2 ([`tonic::transport::Channel`]) or QUIC/HTTP3
//! ([`tonic-h3`](https://docs.rs/tonic-h3)'s `H3Channel`), unified behind one
//! type so the generated client wrappers in this module don't need a
//! per-transport variant.
//!
//! All generated clients use the recording service below, including lazy
//! Direct channels and both authenticated and isolated Onion channels.
//! [`GrpcClientPool::dial`](super::client_pool::GrpcClientPool) owns
//! the probe/fallback policy; this module only knows how to build one QUIC
//! connection and how to present both transports as one `tower::Service`.

mod diagnostics;
#[cfg(feature = "quic")]
mod quic_lifecycle;

use diagnostics::{Family, Phase, Route};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[cfg(feature = "quic")]
use quic_enabled::RawTransport;
#[cfg(feature = "quic")]
pub use quic_enabled::{build_quic, probe_quic};

#[cfg(not(feature = "quic"))]
type RawTransport = tonic::transport::Channel;

use crate::network::connection::{ServerStatus, set_server_status};
use http::{Request, Response};
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::body::Body;

/// Records only evidence from an actual request; construction performs no I/O.
#[derive(Clone)]
pub struct RecordingTransport<S, R = fn(ServerStatus)> {
    inner: S,
    record: R,
    route: Route,
    failure_scope: FailureScope,
}

#[derive(Clone)]
enum FailureScope {
    Unrestricted,
    #[cfg(feature = "onion")]
    Onion(std::sync::Arc<parking_lot::RwLock<super::client_pool::TransportMode>>),
}

impl FailureScope {
    fn record(&self, record: impl FnOnce(ServerStatus)) {
        match self {
            Self::Unrestricted => record(ServerStatus::Unreachable),
            #[cfg(feature = "onion")]
            Self::Onion(mode) => {
                // The handle can survive a route switch. Use the pool's
                // current mode at publication, not the mode when it was built.
                let mode = mode.read();
                if *mode == super::client_pool::TransportMode::Onion {
                    record(ServerStatus::Unreachable);
                }
            }
        }
    }
}

pub type GrpcTransport = RecordingTransport<RawTransport>;

impl GrpcTransport {
    #[cfg(feature = "onion")]
    pub(crate) fn from_onion(
        channel: tonic::transport::Channel,
        pool_mode: std::sync::Arc<parking_lot::RwLock<super::client_pool::TransportMode>>,
    ) -> Self {
        Self {
            route: Route::Tor,
            failure_scope: FailureScope::Onion(pool_mode),
            ..channel.into()
        }
    }

    /// The transport selected for this channel, not a reachability verdict.
    pub fn is_http3(&self) -> bool {
        #[cfg(feature = "quic")]
        {
            matches!(self.inner, RawTransport::Quic(_))
        }
        #[cfg(not(feature = "quic"))]
        {
            false
        }
    }
}

impl From<tonic::transport::Channel> for GrpcTransport {
    fn from(channel: tonic::transport::Channel) -> Self {
        tracing::event!(target: "network_transport::tcp_channel_created", tracing::Level::INFO, {});
        Self {
            inner: channel.into(),
            record: set_server_status,
            route: Route::H2,
            failure_scope: FailureScope::Unrestricted,
        }
    }
}

impl<S, R> tower::Service<Request<Body>> for RecordingTransport<S, R>
where
    S: tower::Service<Request<Body>, Response = Response<Body>>,
    S::Error: Into<BoxError>,
    S::Future: Send + 'static,
    R: Fn(ServerStatus) + Clone + Send + Unpin + 'static,
{
    type Response = Response<Body>;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(|error| {
            let error = error.into();
            self.failure_scope.record(&self.record);
            // Tower readiness runs before a request exists.
            diagnostics::failure(self.route, Phase::Ready, Family::Other, error.as_ref());
            error
        })
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let device_auth = if requires_device_auth(request.uri().path()) {
            request
                .extensions()
                .get::<super::interceptors::DeviceAuthRequest>()
                .cloned()
        } else {
            None
        };
        let family = Family::from_path(request.uri().path());
        let route = self.route;
        let failure_scope = self.failure_scope.clone();
        let future = self.inner.call(request);
        let record = self.record.clone();
        Box::pin(async move {
            match future.await {
                Ok(response) => {
                    // One confirmation per protocol/process, from a real reply.
                    // Never log headers, credentials, paths or message bodies.
                    static SEEN_PROTOCOLS: std::sync::atomic::AtomicU8 =
                        std::sync::atomic::AtomicU8::new(0);
                    let (bit, protocol) = match response.version() {
                        http::Version::HTTP_3 => (1, "h3"),
                        http::Version::HTTP_2 => (2, "h2"),
                        _ => (0, "other"),
                    };
                    if bit != 0
                        && SEEN_PROTOCOLS.fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit
                            == 0
                    {
                        match protocol {
                            "h3" => {
                                tracing::info!(target: "network_transport::h3_response", "RPC transport confirmed by server response")
                            }
                            "h2" => {
                                tracing::info!(target: "network_transport::h2_response", "RPC transport confirmed by server response")
                            }
                            _ => {}
                        }
                    }
                    // An HTTP response, including grpc-status in headers or
                    // trailers, proves contact. Unavailable/DeadlineExceeded
                    // sent by the server are application replies, not outages.
                    record(ServerStatus::Reachable);
                    if let Some(auth) = &device_auth {
                        auth.observe(response.headers());
                    }
                    Ok(response.map(|inner| {
                        Body::new(RecordingBody {
                            inner,
                            record,
                            device_auth,
                            family,
                            route,
                            failure_scope,
                        })
                    }))
                }
                Err(error) => {
                    let error = error.into();
                    failure_scope.record(record);
                    diagnostics::failure(route, Phase::Call, family, error.as_ref());
                    Err(error)
                }
            }
        })
    }
}

// Mirror ProductionPublicMethods: rejecting a public credential/proof is not
// evidence that the current device session ended, even if Basic was attached.
fn requires_device_auth(path: &str) -> bool {
    !matches!(
        path,
        "/chat.v1.AuthService/RequestRegistrationChallenge"
            | "/chat.v1.AuthService/LoginStart"
            | "/chat.v1.AuthService/LoginFinish"
            | "/chat.v1.AuthService/Register"
            | "/chat.v1.AuthService/UseRecoveryCodeStart"
            | "/chat.v1.AuthService/UseRecoveryCodeFinish"
            | "/chat.v1.mailbox.EphemeralMailboxService/CreateEphemeralMailbox"
            | "/chat.v1.mailbox.EphemeralMailboxService/GetMailboxChallenge"
            | "/chat.v1.mailbox.EphemeralMailboxService/PollEphemeralMailbox"
            | "/chat.v1.mailbox.EphemeralMailboxService/SendToEphemeralMailbox"
            | "/chat.v1.mailbox.EphemeralMailboxService/AcknowledgeMessages"
            | "/chat.v1.mailbox.EphemeralMailboxService/GetBlindSignaturePublicKey"
            | "/chat.v1.FileService/GetAttachmentUploadUrl"
            | "/chat.v1.FileService/ConfirmAttachmentUpload"
            | "/chat.v1.FileService/GetAttachmentDownloadUrl"
            | "/chat.v1.FileService/CreateAttachmentMultipartUpload"
            | "/chat.v1.FileService/CompleteAttachmentMultipartUpload"
            | "/chat.v1.FileService/AbortAttachmentMultipartUpload"
            | "/chat.v1.provisioning.DeviceProvisioningService/StartProvisioning"
            | "/chat.v1.provisioning.DeviceProvisioningService/GetProvisioningMessage"
            | "/chat.v1.provisioning.DeviceProvisioningService/CompleteDeviceLinking"
            | "/chat.v1.provisioning.DeviceProvisioningService/GetTransferArchive"
            | "/chat.v1.KeyTransparencyService/GetProof"
            | "/chat.v1.KeyTransparencyService/GetConsistencyProof"
    )
}

struct RecordingBody<R> {
    inner: Body,
    record: R,
    device_auth: Option<super::interceptors::DeviceAuthRequest>,
    family: Family,
    route: Route,
    failure_scope: FailureScope,
}

impl<R: Fn(ServerStatus) + Unpin> http_body::Body for RecordingBody<R> {
    type Data = bytes::Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &frame {
            if let (Some(auth), Some(trailers)) = (&this.device_auth, frame.trailers_ref()) {
                auth.observe(trailers);
            }
        }
        // A failed response stream is local transport evidence. A gRPC error
        // trailer is a successful frame and retains Reachable, regardless of
        // its status code or message. Never inspect/log auth or payload data.
        if let Poll::Ready(Some(Err(error))) = &frame {
            this.failure_scope.record(&this.record);
            diagnostics::failure(this.route, Phase::Body, this.family, error);
        } else if matches!(&frame, Poll::Ready(Some(Ok(_)))) {
            (this.record)(ServerStatus::Reachable);
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(feature = "quic")]
mod quic_enabled {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use http::{Request, Response};
    use tonic::body::Body;

    use super::BoxError;

    use super::quic_lifecycle::TrackedConnector;

    pub type QuicChannel = tonic_h3::H3Channel<TrackedConnector>;

    /// Unified gRPC transport: plain TCP/HTTP2 or QUIC/HTTP3.
    #[derive(Clone)]
    pub enum RawTransport {
        Tcp(tonic::transport::Channel),
        Quic(QuicChannel),
    }

    impl From<tonic::transport::Channel> for RawTransport {
        fn from(channel: tonic::transport::Channel) -> Self {
            RawTransport::Tcp(channel)
        }
    }

    impl tower::Service<Request<Body>> for RawTransport {
        type Response = Response<Body>;
        type Error = BoxError;
        type Future =
            Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            match self {
                RawTransport::Tcp(ch) => tower::Service::poll_ready(ch, cx).map_err(BoxError::from),
                RawTransport::Quic(ch) => {
                    tower::Service::poll_ready(ch, cx).map_err(BoxError::from)
                }
            }
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            match self {
                RawTransport::Tcp(ch) => {
                    let fut = tower::Service::call(ch, req);
                    Box::pin(async move { fut.await.map_err(BoxError::from) })
                }
                RawTransport::Quic(ch) => {
                    let fut = tower::Service::call(ch, req);
                    Box::pin(async move {
                        // H3Channel's Error is already BoxError (tonic_h3::Error).
                        let resp = fut.await?;
                        // H3Channel's response body (H3IncomingClient) is a
                        // different concrete type than Channel's; box both
                        // into the same tonic::body::Body so callers see one
                        // Response type regardless of which arm ran.
                        Ok(resp.map(Body::new))
                    })
                }
            }
        }
    }

    /// Build a quinn client config trusting ONLY the pinned chatter CA, with
    /// post-quantum key exchange only — the same `network::pq_tls` config as
    /// the TCP gRPC connector, storage HTTP and the Tor-tunnelled TLS.
    fn quic_client_config() -> Result<quinn::ClientConfig, BoxError> {
        let mut tls_config = crate::network::pq_tls::pinned_client_config()?;
        // Required for QUIC/h3 protocol negotiation — without it the peer has
        // nothing to select and the handshake fails.
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|e| BoxError::from(e.to_string()))?;
        let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));

        // Without this, quinn's default TransportConfig sends no keepalive
        // at all, and the server's QUIC listener (quic-go, http3.Server)
        // uses its default MaxIdleTimeout of 30s with KeepAlivePeriod
        // disabled — an idle long-lived stream (the message/notification
        // inbox streams sit quiet between events) gets dropped by the
        // server every ~30s. The TCP/HTTP2 path already keeps its
        // connections alive the same way (http2_keep_alive_interval(30s) in
        // client_pool.rs); this is the QUIC-side equivalent. 10s, safely
        // under the server's 30s idle timeout so a PING always lands before
        // the peer would otherwise close it.
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(Duration::from_secs(10)));
        config.transport_config(Arc::new(transport));

        Ok(config)
    }

    /// tonic-h3's connector also resolves the literal authority, so normalize
    /// the URI for both the probe and actual RPCs. A normal HTTPS URL omits
    /// :443; lookup_host("chat.offai.app") otherwise fails before sending UDP.
    fn quic_uri(uri: http::Uri) -> Result<http::Uri, BoxError> {
        if uri.scheme_str() != Some("https") || uri.host().is_none() {
            return Err("QUIC requires an HTTPS origin".into());
        }
        if uri.port_u16().is_some() {
            return Ok(uri);
        }
        let authority = format!("{}:443", uri.authority().ok_or("missing authority")?);
        let mut parts = uri.into_parts();
        parts.authority = Some(authority.parse()?);
        Ok(http::Uri::from_parts(parts)?)
    }

    // URI authorities retain IPv6 brackets, but TLS server names must not.
    fn quic_server_name(uri: &http::Uri) -> Result<&str, BoxError> {
        let host = uri.host().ok_or("QUIC endpoint URI has no host")?;
        Ok(host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host))
    }

    /// Resolve the normalized host:port, preserving an explicit port.
    async fn resolve(uri: &http::Uri) -> Result<std::net::SocketAddr, BoxError> {
        let normalized = quic_uri(uri.clone())?;
        let authority = normalized
            .authority()
            .ok_or("QUIC endpoint URI has no host")?
            .as_str();
        if let Ok(addr) = authority.parse() {
            return Ok(addr);
        }
        tokio::net::lookup_host(authority)
            .await?
            .next()
            .ok_or_else(|| BoxError::from(format!("no addresses resolved for {authority}")))
    }

    #[cfg(test)]
    mod endpoint_tests {
        use super::*;

        #[tokio::test]
        async fn https_without_explicit_port_uses_443() {
            let address = resolve(&"https://127.0.0.1".parse().unwrap())
                .await
                .expect("a normal HTTPS origin must be probeable over QUIC");
            assert_eq!(address, "127.0.0.1:443".parse().unwrap());
        }

        #[tokio::test]
        async fn explicit_https_port_is_preserved() {
            let address = resolve(&"https://127.0.0.1:9443".parse().unwrap())
                .await
                .unwrap();
            assert_eq!(address, "127.0.0.1:9443".parse().unwrap());
        }

        #[tokio::test]
        async fn ipv6_https_without_port_uses_443() {
            let address = resolve(&"https://[::1]".parse().unwrap()).await.unwrap();
            assert_eq!(address, "[::1]:443".parse().unwrap());
        }

        #[tokio::test]
        async fn hostname_https_defaults_to_443() {
            let address = resolve(&"https://localhost".parse().unwrap())
                .await
                .unwrap();
            assert_eq!(address.port(), 443);
            assert!(address.ip().is_loopback());
        }

        #[test]
        fn ipv6_tls_name_does_not_include_uri_brackets() {
            let uri = quic_uri("https://[::1]".parse().unwrap()).unwrap();
            assert_eq!(quic_server_name(&uri).unwrap(), "::1");
        }

        #[test]
        fn rpc_connector_gets_the_same_explicit_port_as_the_probe() {
            let uri = quic_uri("https://chat.offai.app/rpc?x=1".parse().unwrap()).unwrap();
            assert_eq!(uri.authority().unwrap().as_str(), "chat.offai.app:443");
            assert_eq!(uri.path_and_query().unwrap().as_str(), "/rpc?x=1");
            assert!(quic_uri("http://chat.offai.app".parse().unwrap()).is_err());
        }
    }

    /// Bind a local QUIC client endpoint trusting only the pinned chatter CA.
    fn client_endpoint() -> Result<quinn::Endpoint, BoxError> {
        let client_config = quic_client_config()?;
        let mut endpoint =
            quinn::Endpoint::client("[::]:0".parse().expect("valid unspecified addr"))?;
        endpoint.set_default_client_config(client_config);
        Ok(endpoint)
    }

    /// Build a QUIC transport for `uri`. Binds a local UDP socket and
    /// constructs the connector/channel but performs no network I/O — like
    /// `Channel::connect_lazy()`, the real handshake happens on the first
    /// RPC (`H3QuinnConnector::connect` resolves and dials then, retrying
    /// each resolved address). Always succeeds even when nothing answers on
    /// the other end; use [`probe_quic`] first to find that out.
    pub fn build_quic(uri: http::Uri) -> Result<super::GrpcTransport, BoxError> {
        let uri = quic_uri(uri)?;
        let host = quic_server_name(&uri)?.to_string();
        let endpoint = client_endpoint()?;
        let connector = TrackedConnector(tonic_h3::quinn::H3QuinnConnector::new(
            uri.clone(),
            host,
            endpoint,
        ));
        Ok(super::RecordingTransport {
            inner: RawTransport::Quic(tonic_h3::H3Channel::new(connector, uri, None)),
            record: crate::network::connection::set_server_status as fn(super::ServerStatus),
            route: super::Route::H3,
            failure_scope: super::FailureScope::Unrestricted,
        })
    }

    /// Probe whether `uri` is reachable over QUIC/HTTP3: resolves and
    /// performs a real handshake, then discards the connection. This is the
    /// only trustworthy "QUIC works" signal — [`build_quic`] always succeeds
    /// even when nothing answers, since it does no I/O. Callers wrap this in
    /// their own timeout: QUIC/UDP is blocked on some networks (hotel wifi,
    /// corporate firewalls) and a hung handshake must not block forever.
    pub async fn probe_quic(uri: &http::Uri) -> Result<(), BoxError> {
        let host = quic_server_name(&uri)?.to_string();
        let addr = resolve(uri).await?;
        let endpoint = client_endpoint()?;
        endpoint.connect(addr, &host)?.await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod outcome_tests {
    use super::*;
    use crate::network::grpc::interceptors::AuthInterceptor;
    use chatter_proto::chat_v1::{
        GetDeviceMessagesRequest, message_service_client::MessageServiceClient,
    };
    use http_body::Frame;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Frames(VecDeque<Result<Frame<bytes::Bytes>, tonic::Status>>);
    impl http_body::Body for Frames {
        type Data = bytes::Bytes;
        type Error = tonic::Status;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    #[derive(Clone)]
    struct Reply {
        code: Option<tonic::Code>,
        transport_failure: bool,
        body_failure: bool,
    }
    impl tower::Service<Request<Body>> for Reply {
        type Response = Response<Body>;
        type Error = BoxError;
        type Future = std::future::Ready<Result<Self::Response, BoxError>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: Request<Body>) -> Self::Future {
            assert!(req.uri().path().ends_with("/GetDeviceMessages"));
            if self.transport_failure {
                return std::future::ready(Err(std::io::Error::from(
                    std::io::ErrorKind::ConnectionReset,
                )
                .into()));
            }
            let mut frames = VecDeque::new();
            if self.body_failure {
                frames.push_back(Err(tonic::Status::unavailable("response stream reset")));
            } else if self.code.is_none() {
                // Empty protobuf GetDeviceMessagesResponse with a gRPC frame.
                frames.push_back(Ok(Frame::data(bytes::Bytes::from_static(&[0, 0, 0, 0, 0]))));
            }
            let mut trailers = http::HeaderMap::new();
            trailers.insert(
                "grpc-status",
                http::HeaderValue::from_str(
                    &(self.code.unwrap_or(tonic::Code::Ok) as i32).to_string(),
                )
                .unwrap(),
            );
            frames.push_back(Ok(Frame::trailers(trailers)));
            std::future::ready(Ok(Response::builder()
                .header("content-type", "application/grpc")
                .body(Body::new(Frames(frames)))
                .unwrap()))
        }
    }

    // Runs the generated inbox RPC with the same interceptor/recording boundary
    // used by MessageClient, without dialing or starting a server/Tor service.
    async fn inbox(
        reply: Reply,
    ) -> (
        Result<tonic::Response<chatter_proto::chat_v1::GetDeviceMessagesResponse>, tonic::Status>,
        Vec<ServerStatus>,
    ) {
        let evidence = Arc::new(Mutex::new(vec![ServerStatus::Unreachable]));
        let captured = evidence.clone();
        let transport = RecordingTransport {
            inner: reply,
            route: Route::H2,
            failure_scope: FailureScope::Unrestricted,
            record: move |status| captured.lock().unwrap().push(status),
        };
        let mut client = MessageServiceClient::with_interceptor(transport, AuthInterceptor::new());
        let result = client
            .get_device_messages(GetDeviceMessagesRequest {
                after_id: 0,
                limit: 1,
            })
            .await;
        let observed = evidence.lock().unwrap().clone();
        (result, observed)
    }

    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn transport_route_outage_tor_failure_uses_current_pool_mode() {
        use super::super::client_pool::TransportMode;

        let mode = Arc::new(parking_lot::RwLock::new(TransportMode::Direct));
        let scope = FailureScope::Onion(mode.clone());
        for active_mode in [TransportMode::Direct, TransportMode::Onion] {
            // Reuse the scope across switches, as cached clients and response
            // bodies hold onto clones after borrowing their channel.
            *mode.write() = active_mode;
            for transport_failure in [true, false] {
                let observed = Arc::new(Mutex::new(Vec::new()));
                let captured = observed.clone();
                let transport = RecordingTransport {
                    inner: Reply {
                        code: None,
                        transport_failure,
                        body_failure: !transport_failure,
                    },
                    record: move |status| captured.lock().unwrap().push(status),
                    route: Route::Tor,
                    failure_scope: scope.clone(),
                };
                let mut client = MessageServiceClient::new(transport);
                assert!(
                    client
                        .get_device_messages(GetDeviceMessagesRequest {
                            after_id: 0,
                            limit: 1
                        })
                        .await
                        .is_err()
                );
                let saw_unreachable = observed
                    .lock()
                    .unwrap()
                    .contains(&ServerStatus::Unreachable);
                assert_eq!(saw_unreachable, active_mode == TransportMode::Onion);
            }
        }
    }

    /// Controlled peer used by authority regressions: generated production RPC,
    /// interceptor and recording service, with only the wire reply substituted.
    pub(crate) async fn authoritative_inbox(
        code: Option<tonic::Code>,
        transport_failure: bool,
    ) -> Result<(), tonic::Status> {
        let transport = RecordingTransport {
            inner: Reply {
                code,
                transport_failure,
                body_failure: false,
            },
            route: Route::H2,
            failure_scope: FailureScope::Unrestricted,
            record: set_server_status as fn(ServerStatus),
        };
        let mut client = MessageServiceClient::with_interceptor(transport, AuthInterceptor::new());
        client
            .get_device_messages(GetDeviceMessagesRequest {
                after_id: 0,
                limit: 1,
            })
            .await
            .map(|_| ())
    }

    #[tokio::test]
    async fn direct_pool_factories_install_the_production_recorder_without_dialing() {
        let pool = crate::network::grpc::GrpcClientPool::new("https://127.0.0.1:9").unwrap();
        for channel in [pool.channel(), pool.unidentified_channel()] {
            assert!(std::ptr::fn_addr_eq(
                channel.record,
                set_server_status as fn(ServerStatus)
            ));
        }
    }

    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn onion_and_isolated_anonymous_factories_install_the_same_recorder_without_dialing() {
        let pool = crate::network::grpc::GrpcClientPool::new("https://127.0.0.1:9").unwrap();
        pool.set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
        pool.set_transport_mode(crate::network::grpc::client_pool::TransportMode::Onion)
            .unwrap();
        for channel in [pool.channel(), pool.unidentified_channel()] {
            assert!(std::ptr::fn_addr_eq(
                channel.record,
                set_server_status as fn(ServerStatus)
            ));
        }
    }

    #[tokio::test]
    async fn actual_inbox_boundary_success_heals_stale_outage() {
        let (result, observed) = inbox(Reply {
            code: None,
            transport_failure: false,
            body_failure: false,
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(observed.last(), Some(&ServerStatus::Reachable));
    }

    #[tokio::test]
    async fn actual_inbox_boundary_application_status_proves_contact() {
        for code in [
            tonic::Code::Unauthenticated,
            tonic::Code::PermissionDenied,
            tonic::Code::NotFound,
            tonic::Code::Unavailable,
            tonic::Code::DeadlineExceeded,
        ] {
            let (result, observed) = inbox(Reply {
                code: Some(code),
                transport_failure: false,
                body_failure: false,
            })
            .await;
            assert_eq!(result.unwrap_err().code(), code);
            assert_eq!(observed.last(), Some(&ServerStatus::Reachable));
            assert!(!observed[1..].contains(&ServerStatus::Unreachable));
        }
    }

    #[tokio::test]
    async fn actual_inbox_boundary_transport_and_body_failure_report_outage() {
        for (transport_failure, body_failure) in [(true, false), (false, true)] {
            let (result, observed) = inbox(Reply {
                code: None,
                transport_failure,
                body_failure,
            })
            .await;
            assert!(result.is_err());
            assert_eq!(observed.last(), Some(&ServerStatus::Unreachable));
        }
    }

    #[tokio::test]
    async fn local_interceptor_rejection_does_not_claim_server_contact() {
        let evidence = Arc::new(Mutex::new(Vec::new()));
        let captured = evidence.clone();
        let transport = RecordingTransport {
            inner: Reply {
                code: None,
                transport_failure: false,
                body_failure: false,
            },
            route: Route::H2,
            failure_scope: FailureScope::Unrestricted,
            record: move |status| captured.lock().unwrap().push(status),
        };
        let mut client =
            MessageServiceClient::with_interceptor(transport, |_: tonic::Request<()>| {
                Err(tonic::Status::unauthenticated("local rejection"))
            });
        let result = client
            .get_device_messages(GetDeviceMessagesRequest {
                after_id: 0,
                limit: 1,
            })
            .await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        assert!(evidence.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn readiness_failure_reports_outage_without_a_call() {
        struct NotReady;
        impl tower::Service<Request<Body>> for NotReady {
            type Response = Response<Body>;
            type Error = BoxError;
            type Future = std::future::Ready<Result<Self::Response, BoxError>>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
                Poll::Ready(Err(
                    std::io::Error::from(std::io::ErrorKind::NotConnected).into()
                ))
            }
            fn call(&mut self, _: Request<Body>) -> Self::Future {
                panic!("must not call an unready transport")
            }
        }
        let evidence = Arc::new(Mutex::new(Vec::new()));
        let captured = evidence.clone();
        let mut transport = RecordingTransport {
            inner: NotReady,
            route: Route::H2,
            failure_scope: FailureScope::Unrestricted,
            record: move |status| captured.lock().unwrap().push(status),
        };
        let result =
            futures::future::poll_fn(|cx| tower::Service::poll_ready(&mut transport, cx)).await;
        assert!(result.is_err());
        assert_eq!(*evidence.lock().unwrap(), vec![ServerStatus::Unreachable]);
    }
}
