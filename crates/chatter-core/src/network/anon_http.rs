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
//! against the pinned chatter CA. Cert verification is never disabled.
//!
//! # Structure
//! The [`ANON_HTTP_ACTIVE`] flag and URL parsing are always compiled so the leaf
//! media modules (gif) can gate on anon state without
//! pulling in the `onion` feature. The actual Tor transport ([`get`] / [`put`])
//! is gated behind `onion`.

use std::sync::atomic::AtomicBool;

/// Process-global "anon HTTP is active" flag.
///
/// Set true by the client when anon mode is enabled AND the transport actually
/// switched to Onion; false otherwise (including every failure path). Leaf media
/// modules that have no gRPC pool handle (gif) read this to
/// hard-disable their third-party HTTP fetches in anon mode. Under a build
/// without the `onion` feature the transport can never be Onion, so this stays
/// false and every anon branch is dead.
pub static ANON_HTTP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Maximum encrypted storage object, including log-bucket padding, CBC
/// alignment, IV and MAC. Do not trust a response's Content-Length for allocation.
pub(crate) fn storage_response_limit() -> usize {
    let size = crate::crypto::attachment_crypto::max_attachment_size();
    usize::try_from(size.saturating_add(size / 20).saturating_add(1024)).unwrap_or(usize::MAX)
}

pub(crate) fn append_storage_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
) -> Result<(), String> {
    let len = body
        .len()
        .checked_add(chunk.len())
        .filter(|len| *len <= limit)
        .ok_or_else(|| "storage response exceeds attachment size limit".to_string())?;
    body.try_reserve(len - body.len())
        .map_err(|_| "not enough memory for storage response".to_string())?;
    body.extend_from_slice(chunk);
    Ok(())
}

/// Account before writing: oversize/truncated transfers never replace an
/// existing encrypted attachment. The caller owns atomic staging/cleanup.
pub(crate) async fn write_storage_chunk<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    total: u64,
    chunk: &[u8],
    limit: u64,
) -> Result<u64, String> {
    use tokio::io::AsyncWriteExt;
    let next = total
        .checked_add(chunk.len() as u64)
        .filter(|n| *n <= limit)
        .ok_or_else(|| "storage response exceeds attachment size limit".to_string())?;
    writer
        .write_all(chunk)
        .await
        .map_err(|_| "storage file write failed".to_string())?;
    Ok(next)
}

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
    // A Tor exit does not authenticate cleartext HTTP. Only onion services
    // provide authenticated transport without TLS.
    if !https && !host.ends_with(".onion") {
        return Err("storage URL requires HTTPS or an onion service".to_string());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("storage URL must not contain credentials".to_string());
    }
    // `port_or_known_default` fills in 443/80 for https/http when absent.
    let port = u
        .port_or_known_default()
        .unwrap_or(if https { 443 } else { 80 });
    let mut path_and_query = u.path().to_string();
    if let Some(q) = u.query() {
        path_and_query.push('?');
        path_and_query.push_str(q);
    }
    Ok(ParsedUrl {
        https,
        host,
        port,
        path_and_query,
    })
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
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    use arti_client::TorClient;

    use super::{ParsedUrl, PutResponse, parse_url};
    use crate::network::grpc::onion_transport::SharedTorClient;

    struct TorTask<T>(tokio::task::JoinHandle<T>);
    impl<T> Drop for TorTask<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

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
        use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};

        let mut roots = RootCertStore::empty();
        if let Some(ca_pem) = crate::config::TLS_ROOT_CA_PEM {
            // A malformed provisioned PEM must not panic a live request: on parse
            // failure we log and leave the trust store empty, so the handshake
            // fails closed with a clean "no trust anchor" TLS error instead. The
            // dev/test CA is validated by `config::tls_root_ca_is_a_wellformed_pem_certificate`,
            // and X.509 cannot be parsed in the const build-time guard.
            for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                match cert {
                    Ok(cert) => {
                        if let Err(e) = roots.add(cert) {
                            tracing::error!(error = %e, "pinned chatter TLS CA is not a valid trust anchor");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "pinned chatter TLS CA PEM failed to parse");
                    }
                }
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
    /// `isolated`: dial on a fresh isolation group. arti keeps onion-service
    /// state (descriptor, intro/rendezvous circuits) per (service, isolation)
    /// and reuses any circuit that is not closing, so a stalled one keeps
    /// being handed back on the shared group; a new group builds its own.
    async fn connect_tor(
        tor: &SharedTorClient,
        host: &str,
        port: u16,
        isolated: bool,
    ) -> Result<arti_client::DataStream, String> {
        use crate::network::grpc::onion_transport::tor_runtime;
        let base = tor
            .get_or_try_init(|| async {
                let config = crate::network::grpc::onion_transport::tor_client_config()?;
                let mut task = TorTask(tor_runtime().spawn(async {
                    let _ =
                        tokio_rustls::rustls::crypto::ring::default_provider().install_default();
                    TorClient::create_bootstrapped(config).await
                }));
                match tokio::time::timeout(std::time::Duration::from_secs(120), &mut task.0).await {
                    Err(_) => Err("Tor bootstrap timed out after 120s".to_string()),
                    Ok(Err(join)) => Err(format!("Tor bootstrap task panicked: {join}")),
                    Ok(Ok(Err(e))) => Err(format!("Tor bootstrap failed: {e}")),
                    Ok(Ok(Ok(c))) => Ok(c),
                }
            })
            .await?;
        let client = if isolated {
            base.isolated_client()
        } else {
            base.clone()
        };
        let (dial_host, dial_port) = (host.to_string(), port);
        let mut task = TorTask(
            tor_runtime()
                .spawn(async move { client.connect((dial_host.as_str(), dial_port)).await }),
        );
        match tokio::time::timeout(std::time::Duration::from_secs(90), &mut task.0).await {
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
        limit: usize,
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
        let mut incoming = resp.into_body();
        let mut body = Vec::new();
        while let Some(frame) = incoming.frame().await {
            let frame = frame.map_err(|e| format!("reading response body failed: {e}"))?;
            if let Ok(data) = frame.into_data() {
                super::append_storage_chunk(&mut body, &data, limit)?;
            }
        }
        Ok((status, headers, body))
    }

    /// Issue `req` to the parsed URL over Tor, wrapping in TLS for https.
    async fn request(
        tor: &SharedTorClient,
        p: &ParsedUrl,
        req: hyper::Request<Full<Bytes>>,
        limit: usize,
    ) -> Result<(u16, hyper::HeaderMap, Vec<u8>), String> {
        let stream = connect_tor(tor, &p.host, p.port, false).await?;
        if p.https {
            let server_name = ServerName::try_from(p.host.clone())
                .map_err(|e| format!("invalid SNI host {}: {e}", p.host))?;
            let connector = TlsConnector::from(Arc::new(tls_config()));
            let tls = connector
                .connect(server_name, stream)
                .await
                .map_err(|e| format!("TLS handshake to {} failed: {e}", p.host))?;
            send(TokioIo::new(tls), req, limit).await
        } else {
            send(TokioIo::new(stream), req, limit).await
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
        let (status, _headers, body) = request(tor, &p, req, super::storage_response_limit()).await?;
        if !(200..300).contains(&status) {
            return Err(format!("storage GET returned status {status}"));
        }
        Ok(body)
    }

    /// A missing public object means no update has been published yet.
    pub async fn get_optional(tor: &SharedTorClient, url: &str) -> Result<Option<Vec<u8>>, String> {
        let p = parse_url(url)?;
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(&p.path_and_query)
            .header(hyper::header::HOST, host_header(&p))
            .body(Full::new(Bytes::new()))
            .map_err(|_| "update request construction failed".to_string())?;
        let (status, _headers, body) = request(tor, &p, req, crate::update::MAX_MANIFEST_BYTES).await?;
        if status == 404 { return Ok(None); }
        if !(200..300).contains(&status) {
            return Err(format!("update GET returned status {status}"));
        }
        Ok(Some(body))
    }

    /// Stream the authenticated Tor/TLS response into an encrypted staging
    /// file. No redirect, direct fallback or full-body allocation is used.
    pub async fn get_to_file(
        tor: &SharedTorClient,
        url: &str,
        file: &mut tokio::fs::File,
        limit: u64,
    ) -> Result<u64, String> {
        get_to_file_with_progress(tor, url, file, limit, &|_| {}).await
    }

    pub async fn get_to_file_with_progress(
        tor: &SharedTorClient,
        url: &str,
        file: &mut tokio::fs::File,
        limit: u64,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64, String> {
        let p = parse_url(url)?;
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(&p.path_and_query)
            .header(hyper::header::HOST, host_header(&p))
            .body(Full::new(Bytes::new()))
            .map_err(|_| "storage request construction failed".to_string())?;
        // A failed dial is retried once on fresh circuits (RC109: one device
        // failed every storage fetch on the shared ones while another device
        // fetched the same object fine).
        let stream = match connect_tor(tor, &p.host, p.port, false).await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "storage Tor connect failed; retrying on fresh circuits");
                connect_tor(tor, &p.host, p.port, true).await.map_err(|e| {
                    tracing::warn!(error = %e, "storage Tor connect failed on fresh circuits");
                    "storage Tor connection failed".to_string()
                })?
            }
        };
        if p.https {
            let name = ServerName::try_from(p.host.clone())
                .map_err(|_| "storage TLS hostname invalid".to_string())?;
            let tls = TlsConnector::from(Arc::new(tls_config()))
                .connect(name, stream)
                .await
                .map_err(|_| "storage TLS handshake failed".to_string())?;
            send_to_file(TokioIo::new(tls), req, file, limit, progress).await
        } else {
            send_to_file(TokioIo::new(stream), req, file, limit, progress).await
        }
    }

    async fn send_to_file<T>(
        io: TokioIo<T>,
        req: hyper::Request<Full<Bytes>>,
        file: &mut tokio::fs::File,
        limit: u64,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64, String>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|_| "storage HTTP handshake failed".to_string())?;
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let response = sender
            .send_request(req)
            .await
            .map_err(|_| "storage request failed".to_string())?;
        if !response.status().is_success() {
            return Err(format!("storage GET returned status {}", response.status()));
        }
        if response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.parse::<u64>().ok())
            .is_some_and(|len| len > limit)
        {
            return Err("storage response exceeds attachment size limit".into());
        }
        let mut incoming = response.into_body();
        let mut total = 0;
        while let Some(frame) = incoming.frame().await {
            let frame = frame.map_err(|_| "storage response read failed".to_string())?;
            if let Ok(data) = frame.into_data() {
                total = super::write_storage_chunk(file, total, &data, limit).await?;
                progress(total);
            }
        }
        Ok(total)
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
        let (status, headers, _body) = request(tor, &p, req, super::storage_response_limit()).await?;
        let etag = headers
            .get(hyper::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string())
            .filter(|s| !s.is_empty());
        Ok(PutResponse { status, etag })
    }
}

#[cfg(feature = "onion")]
pub use tor::{get, get_optional, get_to_file, get_to_file_with_progress, put};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streamed_ciphertext_limit_rejects_before_writing_or_overflowing() {
        use tokio::io::AsyncWriteExt;
        let staging = tempfile::NamedTempFile::new().unwrap();
        let mut file = tokio::fs::File::from_std(staging.reopen().unwrap());
        let total = write_storage_chunk(&mut file, 0, b"abc", 6).await.unwrap();
        let total = write_storage_chunk(&mut file, total, b"def", 6)
            .await
            .unwrap();
        assert_eq!(total, 6);
        assert!(
            write_storage_chunk(&mut file, total, b"g", 6)
                .await
                .is_err()
        );
        assert!(
            write_storage_chunk(&mut file, u64::MAX, b"x", u64::MAX)
                .await
                .is_err()
        );
        file.flush().await.unwrap();
        file.sync_all().await.unwrap();
        assert_eq!(std::fs::read(staging.path()).unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn streamed_ciphertext_buffered_write_failure_is_detected_before_publish() {
        use tokio::io::AsyncWriteExt;
        let staging = tempfile::NamedTempFile::new().unwrap();
        let mut file = tokio::fs::File::from_std(std::fs::File::open(staging.path()).unwrap());
        let write = write_storage_chunk(&mut file, 0, b"ciphertext", 128).await;
        let flush = file.flush().await;
        assert!(write.is_err() || flush.is_err());
        assert!(std::fs::read(staging.path()).unwrap().is_empty());
    }

    #[test]
    fn storage_response_limit_is_enforced_before_append() {
        let mut body = vec![1, 2];
        append_storage_chunk(&mut body, &[3, 4], 4).unwrap();
        assert!(append_storage_chunk(&mut body, &[5], 4).is_err());
        assert_eq!(body, [1, 2, 3, 4]);
    }

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
        let p = parse_url("http://storage.onion/bucket/key").unwrap();
        assert!(!p.https);
        assert_eq!(p.host, "storage.onion");
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
        let p = parse_url("https://h").unwrap();
        assert_eq!(p.path_and_query, "/");
    }

    #[test]
    fn parse_rejects_unauthenticated_storage() {
        assert!(parse_url("http://storage.local/bucket/key").is_err());
        assert!(parse_url("http://storage.onion.evil.example/key").is_err());
        assert!(parse_url("https://user:secret@storage.example/key").is_err());
    }

    #[test]
    fn parse_rejects_non_http_scheme() {
        assert!(parse_url("ftp://h/x").is_err());
        assert!(parse_url("not a url").is_err());
    }
}
