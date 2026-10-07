//! Explicit private-PKI policy for a single signaling client.

use super::{SignalError, SignalResult};

pub(super) fn http_client(pem: &str) -> SignalResult<reqwest::Client> {
    // Use the socket parser too: reqwest may accept PEM without any certificate.
    connector(Some(pem))?;
    let certificate = reqwest::Certificate::from_pem(pem.as_bytes())
        .map_err(|_| SignalError::UrlParse("invalid pinned signaling CA".into()))?;
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(certificate)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| SignalError::UrlParse("cannot build pinned signaling HTTP client".into()))
}

pub(super) fn connector(pem: Option<&str>) -> SignalResult<Option<tokio_tungstenite::Connector>> {
    let Some(pem) = pem else { return Ok(None) };
    #[cfg(any(
        feature = "__rustls-tls",
        feature = "rustls-tls-native-roots",
        feature = "rustls-tls-webpki-roots"
    ))]
    {
        use std::sync::Arc;
        use tokio_rustls::rustls::{
            self,
            pki_types::{pem::PemObject, CertificateDer},
        };
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let cert =
                cert.map_err(|_| SignalError::UrlParse("invalid pinned signaling CA".into()))?;
            roots
                .add(cert)
                .map_err(|_| SignalError::UrlParse("invalid pinned signaling CA".into()))?;
        }
        if roots.is_empty() {
            return Err(SignalError::UrlParse(
                "missing pinned signaling CA certificate".into(),
            ));
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| SignalError::UrlParse("cannot configure pinned signaling TLS".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Some(tokio_tungstenite::Connector::Rustls(Arc::new(config))))
    }
    #[cfg(not(any(
        feature = "__rustls-tls",
        feature = "rustls-tls-native-roots",
        feature = "rustls-tls-webpki-roots"
    )))]
    {
        let _ = pem;
        Err(SignalError::UrlParse(
            "pinned signaling requires a rustls feature".into(),
        ))
    }
}
