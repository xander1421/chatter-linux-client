//! Network communication layer.
//!
//! All network communication including gRPC clients and real-time streaming.

pub mod anon_http;
pub mod connection;
pub mod grpc;
pub mod pq_tls;
pub mod reconnect_config;
pub mod server_monitor;
pub(crate) mod storage_http;
pub mod streaming;
pub mod validation;

pub use reconnect_config::ReconnectConfig;

/// Public (non-Chatter) HTTPS: WebPKI roots and rustls' default aws-lc-rs
/// provider policy (classical groups plus X25519MLKEM768) — we cannot require
/// PQ from servers we do not run. Explicit configuration also avoids
/// introducing Android platform-verifier JNI setup.
pub(crate) fn public_http_client_builder() -> reqwest::ClientBuilder {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("aws-lc-rs supports the default TLS protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}
