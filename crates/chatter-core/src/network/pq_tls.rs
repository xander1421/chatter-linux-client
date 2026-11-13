//! Post-quantum TLS for every connection pinned to the Chatter CA.
//!
//! Owner decision 2026-11-13: require post-quantum key exchange, no classical
//! fallback. Every client config that trusts `config::TLS_ROOT_CA_PEM` offers
//! exactly one key-exchange group, the X25519 + ML-KEM-768 hybrid, over TLS 1.3
//! only, so a server (or a TLS-terminating middlebox) that cannot do the
//! hybrid fails the handshake instead of silently negotiating plain X25519.
//! A recorded session therefore stays confidential against a future quantum
//! adversary ("harvest now, decrypt later").
//!
//! Endpoints that are not pinned to the Chatter CA (public HTTPS, Nostr
//! relays, Tor's own link TLS) keep rustls' default provider policy: we do not
//! control those servers and cannot require PQ from them.

use std::sync::Arc;

use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use rustls::{ClientConfig, RootCertStore};

/// The pinned-endpoint provider: aws-lc-rs with its key-exchange list reduced
/// to the hybrid group. Cipher suites and signature algorithms stay rustls'
/// defaults.
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        kx_groups: vec![aws_lc_rs::kx_group::X25519MLKEM768],
        ..aws_lc_rs::default_provider()
    }
}

/// A client config trusting only `roots`, PQ-only and TLS 1.3-only. No ALPN
/// is set; callers that need one add it.
pub fn client_config(roots: RootCertStore) -> ClientConfig {
    ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("aws-lc-rs supports TLS 1.3 with X25519MLKEM768")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// Parse a PEM bundle into a trust store. Rejects a malformed bundle and one
/// with no certificate at all.
pub fn roots_from_pem(pem: &str) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        let certificate = certificate.map_err(|e| format!("invalid pinned TLS CA: {e}"))?;
        roots
            .add(certificate)
            .map_err(|e| format!("invalid pinned TLS CA: {e}"))?;
    }
    if roots.is_empty() {
        return Err("no certificates in pinned TLS CA".to_string());
    }
    Ok(roots)
}

/// The trust store for the build's pinned Chatter CA.
pub fn pinned_roots() -> Result<RootCertStore, String> {
    let pem = crate::config::TLS_ROOT_CA_PEM.ok_or_else(|| {
        "no TLS root CA provisioned; refusing to open an unpinned TLS connection \
         (build with CHATTER_TLS_ROOT_CA_PEM, or --features dev-trust-root)"
            .to_string()
    })?;
    roots_from_pem(pem)
}

/// The PQ-only client config for the build's pinned Chatter CA.
pub fn pinned_client_config() -> Result<ClientConfig, String> {
    pinned_roots().map(client_config)
}

/// The direct gRPC connector: TCP with the endpoint's socket settings, then
/// the pinned PQ-only TLS with ALPN `h2`. tonic's `ClientTlsConfig` cannot
/// restrict key-exchange groups, so tonic's TLS feature is off and this
/// connector does the handshake. `http://` passes through untouched (only
/// dev-loopback reaches it, see `GrpcClientPool::validate_direct_url`).
///
/// A CA that fails to parse leaves the trust store empty, so every handshake
/// fails closed; `create_endpoint` reports that error before any channel is
/// built.
pub(crate) fn grpc_connector(
    endpoint: &tonic::transport::Endpoint,
) -> hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector> {
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(endpoint.get_tcp_nodelay());
    http.set_keepalive(endpoint.get_tcp_keepalive());
    http.set_keepalive_interval(endpoint.get_tcp_keepalive_interval());
    http.set_keepalive_retries(endpoint.get_tcp_keepalive_retries());
    http.set_connect_timeout(endpoint.get_connect_timeout());
    let roots = pinned_roots().unwrap_or_else(|_| RootCertStore::empty());
    hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(client_config(roots))
        .https_or_http()
        .enable_http2()
        .wrap_connector(http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, ServerName};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn pinned_config_offers_only_x25519mlkem768() {
        let config = pinned_client_config().expect("test builds pin the dev CA");
        let groups: Vec<_> = config
            .crypto_provider()
            .kx_groups
            .iter()
            .map(|g| g.name())
            .collect();
        assert_eq!(groups, [rustls::NamedGroup::X25519MLKEM768]);
    }

    struct Pki {
        ca_pem: String,
        leaf: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
    }

    fn pki() -> Pki {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .signed_by(&leaf_key, &ca, &ca_key)
            .unwrap();
        Pki {
            ca_pem: ca.pem(),
            leaf: leaf.der().clone(),
            key: PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap(),
        }
    }

    /// One loopback handshake: a rustls server offering `server_groups`
    /// against the production client config for this test CA.
    async fn handshake(
        server_groups: Vec<&'static dyn rustls::crypto::SupportedKxGroup>,
    ) -> Result<(), String> {
        let pki = pki();
        let server_provider = CryptoProvider {
            kx_groups: server_groups,
            ..aws_lc_rs::default_provider()
        };
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(server_provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![pki.leaf], pki.key)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = acceptor.accept(tcp).await {
                let _ = tls.write_all(b"ok").await;
                let _ = tls.shutdown().await;
            }
        });

        let client = client_config(roots_from_pem(&pki.ca_pem)?);
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let result = async {
            let mut tls = connector
                .connect(ServerName::try_from("localhost").unwrap(), tcp)
                .await
                .map_err(|e| e.to_string())?;
            let group = tls
                .get_ref()
                .1
                .negotiated_key_exchange_group()
                .map(|g| g.name());
            assert_eq!(group, Some(rustls::NamedGroup::X25519MLKEM768));
            let mut reply = Vec::new();
            tls.read_to_end(&mut reply)
                .await
                .map_err(|e| e.to_string())?;
            assert_eq!(reply, b"ok");
            Ok(())
        }
        .await;
        let _ = server.await;
        result
    }

    #[tokio::test]
    async fn handshake_succeeds_against_a_hybrid_pq_server() {
        // A server that supports the hybrid alongside classical groups, like
        // Go's and OpenSSL 3.5's defaults.
        handshake(aws_lc_rs::default_provider().kx_groups)
            .await
            .expect("PQ-capable server must accept the PQ-only client");
    }

    #[tokio::test]
    async fn handshake_fails_against_a_classical_only_server() {
        let err = handshake(vec![aws_lc_rs::kx_group::X25519])
            .await
            .expect_err("a server without X25519MLKEM768 must be refused");
        assert!(
            err.contains("HandshakeFailure") || err.contains("handshake"),
            "unexpected error: {err}"
        );
    }
}
