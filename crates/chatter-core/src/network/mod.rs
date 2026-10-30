//! Network communication layer.
//!
//! All network communication including gRPC clients and real-time streaming.

pub mod anon_http;
pub mod connection;
pub mod grpc;
pub mod reconnect_config;
pub mod server_monitor;
pub(crate) mod storage_http;
pub mod streaming;
pub mod validation;

pub use connection::{ConnectionState, FailReason, ServerStatus, server_status};
pub use reconnect_config::ReconnectConfig;

/// Public HTTP retains the WebPKI roots and ring provider used before reqwest 0.13.
/// Explicit configuration also avoids introducing Android platform-verifier JNI setup.
pub(crate) fn public_http_client_builder() -> reqwest::ClientBuilder {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}
