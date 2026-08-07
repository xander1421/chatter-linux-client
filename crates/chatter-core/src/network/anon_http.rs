//! HTTP-over-Tor client for anon mode.
//!
//! In anon mode the gRPC pool routes over Tor, but the direct `reqwest` calls
//! that fetch/store CDN content (attachments, thumbnails, avatars) do NOT — they
//! would leak the client's real IP to the storage/CDN host. This module carries
//! those HTTP requests over the same Arti `TorClient` the gRPC pool uses, so the
//! client IP stays hidden.
//!
//! # What Tor does and does not give us
//! The Tor tunnel hides the client IP from the CDN host; it does **not**
//! authenticate that host. That is still TLS's job — so for `https://` we run a
//! real rustls client handshake with SNI and full certificate verification
//! against the webpki root store. Cert verification is never disabled.
//!
//! # Structure
//! The [`ANON_HTTP_ACTIVE`] flag and URL parsing are always compiled so the leaf
//! media modules (gif / youtube / link_preview) can gate on anon state without
//! pulling in the `onion` feature. The actual Tor transport ([`get`] / [`put`])
//! is gated behind `onion`.

use std::sync::atomic::AtomicBool;

/// Process-global "anon HTTP is active" flag.
///
/// Set true by the client when anon mode is enabled AND the transport actually
/// switched to Onion; false otherwise (including every failure path). Leaf media
/// modules that have no gRPC pool handle (gif/youtube/link_preview) read this to
/// hard-disable their third-party HTTP fetches in anon mode. Under a build
/// without the `onion` feature the transport can never be Onion, so this stays
/// false and every anon branch is dead.
pub static ANON_HTTP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Parsed components of an absolute http(s) URL.
// Used by the `onion` Tor transport below and by unit tests; unused in a plain
// non-onion build.
#[cfg_attr(not(feature = "onion"), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    /// True for `https`, false for `http`.
    pub https: bool,
    /// Host (no port).
    pub host: String,
    /// Port — explicit if present, else the scheme default (443/80).
    pub port: u16,
    /// Origin-form request target: path plus `?query` when present.
    pub path_and_query: String,
}

/// Parse an absolute `http`/`https` URL into host/port/path/scheme.
///
/// Rejects any non-http(s) scheme and any URL without a host.
#[cfg_attr(not(feature = "onion"), allow(dead_code))]
pub(crate) fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let u = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let https = match u.scheme() {
        "https" => true,
        "http" => false,
        other => return Err(format!("unsupported scheme: {other}")),
    };
    let host = u
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_string();
    // `port_or_known_default` fills in 443/80 for https/http when absent.
    let port = u.port_or_known_default().unwrap_or(if https { 443 } else { 80 });
    let mut path_and_query = u.path().to_string();
    if let Some(q) = u.query() {
        path_and_query.push('?');
        path_and_query.push_str(q);
    }
    Ok(ParsedUrl { https, host, port, path_and_query })
}

/// Response of a [`put`]: status code plus the ETag header (needed to complete
/// S3/RustFS multipart uploads).
#[cfg(feature = "onion")]
pub struct PutResponse {
    pub status: u16,
    pub etag: Option<String>,
}

#[cfg(feature = "onion")]
mod tor {
    use std::sync::Arc;

    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    use tokio_rustls::TlsConnector;

    use arti_client::{TorClient, TorClientConfig};

    use super::{parse_url, ParsedUrl, PutResponse};
    use crate::network::grpc::onion_transport::SharedTorClient;

    /// Host header value: bare host, or `host:port` when the port is non-default.
    fn host_header(p: &ParsedUrl) -> String {
        let default = if p.https { 443 } else { 80 };
        if p.port == default {
            p.host.clone()
        } else {
            format!("{}:{}", p.host, p.port)
        }
    }

    /// Build a rustls client config that verifies the server certificate against
    /// the pinned chatter CA — Signal-style private-PKI pin (§8), the SAME
    /// anchor the direct gRPC path uses. This path tunnels ordinary hostname TLS
    /// through a Tor circuit, so it does the identical CA-only trust: only the
    /// provisioned chatter CA, never the webpki/system roots. SNI is supplied at
    /// connect time. Uses the `ring` crypto provider explicitly so we never
    /// depend on a process-global default being installed.
    fn tls_config() -> ClientConfig {
        use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer};

        let mut roots = RootCertStore::empty();
        if let Some(ca_pem) = crate::config::TLS_ROOT_CA_PEM {
            for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                let cert = cert.expect("pinned chatter TLS CA PEM must parse");
                roots
                    .add(cert)
                    .expect("pinned chatter TLS CA must be a valid trust anchor");
            }
        }
        ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
    }

    /// Bootstrap (once) and connect a raw Tor stream to `host:port`.
    ///
    /// This shares the gRPC pool's `SharedTorClient` OnceCell, so the init here
    /// MUST match the gRPC connector's: run on the dedicated Tor runtime (so Arti
    /// never binds to the app's DB-starved runtime, whichever path wins the init
    /// race) and install the ring crypto provider before bootstrap. See
    /// [`crate::network::grpc::onion_transport::tor_runtime`].
    async fn connect_tor(
        tor: &SharedTorClient,
        host: &str,
        port: u16,
    ) -> Result<arti_client::DataStream, String> {
        use crate::network::grpc::onion_transport::tor_runtime;
        let base = tor
            .get_or_try_init(|| async {
                let jh = tor_runtime().spawn(async {
                    let _ = tokio_rustls::rustls::crypto::ring::default_provider()
                        .install_default();
                    TorClient::create_bootstrapped(TorClientConfig::default()).await
                });
                match tokio::time::timeout(std::time::Duration::from_secs(120), jh).await {
                    Err(_) => Err("Tor bootstrap timed out after 120s".to_string()),
                    Ok(Err(join)) => Err(format!("Tor bootstrap task panicked: {join}")),
                    Ok(Ok(Err(e))) => Err(format!("Tor bootstrap failed: {e}")),
                    Ok(Ok(Ok(c))) => Ok(c),
                }
            })
            .await?;
        let client = base.clone();
        let (dial_host, dial_port) = (host.to_string(), port);
        let jh = tor_runtime()
            .spawn(async move { client.connect((dial_host.as_str(), dial_port)).await });
        match tokio::time::timeout(std::time::Duration::from_secs(90), jh).await {
            Err(_) => Err(format!("Tor connect to {host}:{port} timed out after 90s")),
            Ok(Err(join)) => Err(format!("Tor connect task panicked: {join}")),
            Ok(Ok(Err(e))) => Err(format!("Tor connect to {host}:{port} failed: {e}")),
            Ok(Ok(Ok(s))) => Ok(s),
        }
    }

    /// Drive one HTTP/1.1 request over `io` and return (status, headers, body).
    async fn send<T>(
        io: TokioIo<T>,
        req: hyper::Request<Full<Bytes>>,
    ) -> Result<(u16, hyper::HeaderMap, Vec<u8>), String>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| format!("HTTP handshake failed: {e}"))?;
        // The connection must be driven concurrently with sending the request.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(error = %e, "anon_http: connection closed with error");
            }
        });
        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("reading response body failed: {e}"))?
            .to_bytes()
            .to_vec();
        Ok((status, headers, body))
    }

    /// Issue `req` to the parsed URL over Tor, wrapping in TLS for https.
    async fn request(
        tor: &SharedTorClient,
        p: &ParsedUrl,
        req: hyper::Request<Full<Bytes>>,
    ) -> Result<(u16, hyper::HeaderMap, Vec<u8>), String> {
        let stream = connect_tor(tor, &p.host, p.port).await?;
        if p.https {
            let server_name = ServerName::try_from(p.host.clone())
                .map_err(|e| format!("invalid SNI host {}: {e}", p.host))?;
            let connector = TlsConnector::from(Arc::new(tls_config()));
            let tls = connector
                .connect(server_name, stream)
                .await
                .map_err(|e| format!("TLS handshake to {} failed: {e}", p.host))?;
            send(TokioIo::new(tls), req).await
        } else {
            send(TokioIo::new(stream), req).await
        }
    }

    /// HTTP GET over Tor. Verifies the response is 2xx and returns the body.
    pub async fn get(tor: &SharedTorClient, url: &str) -> Result<Vec<u8>, String> {
        let p = parse_url(url)?;
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(&p.path_and_query)
            .header(hyper::header::HOST, host_header(&p))
            .body(Full::new(Bytes::new()))
            .map_err(|e| format!("building GET request failed: {e}"))?;
        let (status, _headers, body) = request(tor, &p, req).await?;
        if !(200..300).contains(&status) {
            return Err(format!("GET {url} returned status {status}"));
        }
        Ok(body)
    }

    /// HTTP PUT over Tor. Returns the status and ETag; the caller decides how to
    /// treat non-2xx (part uploads retry, single PUTs error).
    pub async fn put(
        tor: &SharedTorClient,
        url: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<PutResponse, String> {
        let p = parse_url(url)?;
        let req = hyper::Request::builder()
            .method(hyper::Method::PUT)
            .uri(&p.path_and_query)
            .header(hyper::header::HOST, host_header(&p))
            .header(hyper::header::CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| format!("building PUT request failed: {e}"))?;
        let (status, headers, _body) = request(tor, &p, req).await?;
        let etag = headers
            .get(hyper::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string())
            .filter(|s| !s.is_empty());
        Ok(PutResponse { status, etag })
    }
}

#[cfg(feature = "onion")]
pub use tor::{get, put};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_default_port() {
        let p = parse_url("https://cdn.example.com/path/to/obj?sig=abc").unwrap();
        assert!(p.https);
        assert_eq!(p.host, "cdn.example.com");
        assert_eq!(p.port, 443);
        assert_eq!(p.path_and_query, "/path/to/obj?sig=abc");
    }

    #[test]
    fn parse_http_default_port() {
        let p = parse_url("http://storage.local/bucket/key").unwrap();
        assert!(!p.https);
        assert_eq!(p.host, "storage.local");
        assert_eq!(p.port, 80);
        assert_eq!(p.path_and_query, "/bucket/key");
    }

    #[test]
    fn parse_explicit_non_default_port() {
        let p = parse_url("https://rustfs.internal:9000/b/k").unwrap();
        assert!(p.https);
        assert_eq!(p.host, "rustfs.internal");
        assert_eq!(p.port, 9000);
        assert_eq!(p.path_and_query, "/b/k");
    }

    #[test]
    fn parse_root_path_defaults_to_slash() {
        let p = parse_url("http://h").unwrap();
        assert_eq!(p.path_and_query, "/");
    }

    #[test]
    fn parse_rejects_non_http_scheme() {
        assert!(parse_url("ftp://h/x").is_err());
        assert!(parse_url("not a url").is_err());
    }
}
