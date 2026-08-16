//! `GrpcTransport`: TCP/HTTP2 ([`tonic::transport::Channel`]) or QUIC/HTTP3
//! ([`tonic-h3`](https://docs.rs/tonic-h3)'s `H3Channel`), unified behind one
//! type so the generated client wrappers in this module don't need a
//! per-transport variant.
//!
//! With the `quic` feature off, `GrpcTransport` IS `Channel` — same type,
//! same monomorphization, zero new code on the path every existing build
//! runs. [`GrpcClientPool::dial`](super::client_pool::GrpcClientPool) owns
//! the probe/fallback policy; this module only knows how to build one QUIC
//! connection and how to present both transports as one `tower::Service`.

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[cfg(feature = "quic")]
pub use quic_enabled::*;

#[cfg(not(feature = "quic"))]
pub type GrpcTransport = tonic::transport::Channel;

#[cfg(feature = "quic")]
mod quic_enabled {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use http::{Request, Response};
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    use rustls::{ClientConfig as RustlsClientConfig, RootCertStore};
    use tonic::body::Body;

    use super::BoxError;

    pub type QuicChannel = tonic_h3::H3Channel<tonic_h3::quinn::H3QuinnConnector>;

    /// Unified gRPC transport: plain TCP/HTTP2 or QUIC/HTTP3.
    #[derive(Clone)]
    pub enum GrpcTransport {
        Tcp(tonic::transport::Channel),
        Quic(QuicChannel),
    }

    impl From<tonic::transport::Channel> for GrpcTransport {
        fn from(channel: tonic::transport::Channel) -> Self {
            GrpcTransport::Tcp(channel)
        }
    }

    impl tower::Service<Request<Body>> for GrpcTransport {
        type Response = Response<Body>;
        type Error = BoxError;
        type Future =
            Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            match self {
                GrpcTransport::Tcp(ch) => tower::Service::poll_ready(ch, cx).map_err(BoxError::from),
                GrpcTransport::Quic(ch) => tower::Service::poll_ready(ch, cx).map_err(BoxError::from),
            }
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            match self {
                GrpcTransport::Tcp(ch) => {
                    let fut = tower::Service::call(ch, req);
                    Box::pin(async move { fut.await.map_err(BoxError::from) })
                }
                GrpcTransport::Quic(ch) => {
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

    /// Build a quinn client config trusting ONLY the pinned chatter CA — the
    /// same private-PKI pin (§8) as the TCP path's `ClientTlsConfig`
    /// (`client_pool.rs`) and the Tor-tunnelled TLS in `anon_http.rs`. Three
    /// different wrapper types (tonic's `ClientTlsConfig`, plain
    /// `rustls::ClientConfig`, quinn's `QuicClientConfig`) with no shared
    /// constructor, so the trust-store-building code is duplicated rather
    /// than the trust decision itself (CA PEM comes from the one source of
    /// truth, `config::TLS_ROOT_CA_PEM`, in all three places).
    fn quic_client_config() -> Result<quinn::ClientConfig, BoxError> {
        let ca_pem = crate::config::TLS_ROOT_CA_PEM.ok_or(
            "no TLS root CA provisioned; refusing to open an unpinned QUIC connection \
             (build with CHATTER_TLS_ROOT_CA_PEM, or --features dev-trust-root)",
        )?;
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
            roots.add(cert?)?;
        }
        let mut tls_config = RustlsClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| BoxError::from(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        // Required for QUIC/h3 protocol negotiation — without it the peer has
        // nothing to select and the handshake fails.
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|e| BoxError::from(e.to_string()))?;
        Ok(quinn::ClientConfig::new(Arc::new(quic_crypto)))
    }

    /// Resolve `uri`'s host:port to a socket address. IP-literal authorities
    /// parse directly; DNS names go through the resolver.
    async fn resolve(uri: &http::Uri) -> Result<std::net::SocketAddr, BoxError> {
        let authority = uri.authority().ok_or("QUIC endpoint URI has no host")?.as_str();
        if let Ok(addr) = authority.parse() {
            return Ok(addr);
        }
        tokio::net::lookup_host(authority)
            .await?
            .next()
            .ok_or_else(|| BoxError::from(format!("no addresses resolved for {authority}")))
    }

    /// Bind a local QUIC client endpoint trusting only the pinned chatter CA.
    fn client_endpoint() -> Result<quinn::Endpoint, BoxError> {
        let client_config = quic_client_config()?;
        let mut endpoint = quinn::Endpoint::client("[::]:0".parse().expect("valid unspecified addr"))?;
        endpoint.set_default_client_config(client_config);
        Ok(endpoint)
    }

    /// Build a QUIC transport for `uri`. Binds a local UDP socket and
    /// constructs the connector/channel but performs no network I/O — like
    /// `Channel::connect_lazy()`, the real handshake happens on the first
    /// RPC (`H3QuinnConnector::connect` resolves and dials then, retrying
    /// each resolved address). Always succeeds even when nothing answers on
    /// the other end; use [`probe_quic`] first to find that out.
    pub fn build_quic(uri: http::Uri) -> Result<GrpcTransport, BoxError> {
        let host = uri.host().ok_or("QUIC endpoint URI has no host")?.to_string();
        let endpoint = client_endpoint()?;
        let connector = tonic_h3::quinn::H3QuinnConnector::new(uri.clone(), host, endpoint);
        Ok(GrpcTransport::Quic(tonic_h3::H3Channel::new(
            connector, uri, None,
        )))
    }

    /// Probe whether `uri` is reachable over QUIC/HTTP3: resolves and
    /// performs a real handshake, then discards the connection. This is the
    /// only trustworthy "QUIC works" signal — [`build_quic`] always succeeds
    /// even when nothing answers, since it does no I/O. Callers wrap this in
    /// their own timeout: QUIC/UDP is blocked on some networks (hotel wifi,
    /// corporate firewalls) and a hung handshake must not block forever.
    pub async fn probe_quic(uri: &http::Uri) -> Result<(), BoxError> {
        let host = uri.host().ok_or("QUIC endpoint URI has no host")?.to_string();
        let addr = resolve(uri).await?;
        let endpoint = client_endpoint()?;
        endpoint.connect(addr, &host)?.await?;
        Ok(())
    }
}
