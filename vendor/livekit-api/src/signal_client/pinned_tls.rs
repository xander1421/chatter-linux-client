//! Explicit private-PKI policy for a single signaling client.
//!
//! A pinned signaling connection is a Chatter-operated endpoint, so it follows
//! Chatter's post-quantum TLS policy (`chatter-core` `network::pq_tls`): aws-lc-rs,
//! TLS 1.3 only, and X25519MLKEM768 as the ONLY key-exchange group, so a server
//! that cannot do the hybrid fails the handshake.

use super::{SignalError, SignalResult};

pub(super) fn http_client(pem: &str) -> SignalResult<reqwest::Client> {
    let config = client_config(pem)?;
    reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| SignalError::UrlParse("cannot build pinned signaling HTTP client".into()))
}

pub(super) fn connector(pem: Option<&str>) -> SignalResult<Option<tokio_tungstenite::Connector>> {
    let Some(pem) = pem else { return Ok(None) };
    let config = client_config(pem)?;
    Ok(Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config))))
}

fn client_config(pem: &str) -> SignalResult<tokio_rustls::rustls::ClientConfig> {
    use std::sync::Arc;
    use tokio_rustls::rustls::{
        self,
        crypto::{aws_lc_rs, CryptoProvider},
        pki_types::{pem::PemObject, CertificateDer},
    };
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        let cert = cert.map_err(|_| SignalError::UrlParse("invalid pinned signaling CA".into()))?;
        roots.add(cert).map_err(|_| SignalError::UrlParse("invalid pinned signaling CA".into()))?;
    }
    if roots.is_empty() {
        return Err(SignalError::UrlParse("missing pinned signaling CA certificate".into()));
    }
    let provider = CryptoProvider {
        kx_groups: vec![aws_lc_rs::kx_group::X25519MLKEM768],
        ..aws_lc_rs::default_provider()
    };
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| SignalError::UrlParse("cannot configure pinned signaling TLS".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}
