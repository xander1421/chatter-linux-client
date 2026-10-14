//! Self-hosted ntfy push listener.
//!
//! Connects to a self-hosted ntfy instance via SSE (Server-Sent Events) for
//! push notifications on Android. Desktop clients don't need this — they use
//! persistent gRPC streams directly.
//!
//! # Privacy
//!
//! The backend sends either content-free ("New activity") or full notifications
//! depending on the server's `NTFY_PUSH_MODE` setting. The client always wakes
//! up and pulls actual message content via the encrypted gRPC stream.
//!
//! # Architecture
//!
//! ```text
//! Self-hosted ntfy ──[SSE]──> PushListener ──> wake gRPC streams / inbox poll
//!                                           (payload is never rendered as an alert)
//! ```

use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::network::ReconnectConfig;

/// Configuration for the ntfy push listener.
#[derive(Clone)]
pub struct PushListenerConfig {
    /// Private-CA HTTPS or v3 onion base URL. Development builds also allow
    /// explicit loopback HTTP on port 8095.
    pub ntfy_url: String,

    /// Anonymous mailbox subscriptions always use isolated Tor, including HTTPS.
    pub force_tor: bool,

    /// Opaque server-issued subscription capability. Never log it.
    pub topic: String,

    /// Optional auth token for subscribing to the topic.
    pub auth_token: Option<String>,

    /// Initial reconnect delay on connection failure. Kept as a field (rather
    /// than derived from `reconnect.initial_delay`) for API backwards compat —
    /// older callers that set `reconnect_delay` before the `ReconnectConfig`
    /// refactor still see it honored.
    pub reconnect_delay: Duration,

    /// Backoff / give-up tuning for the reconnect loop. Defaults reproduce the
    /// historical hard-coded behavior exactly (5s initial, 60s cap, 2x
    /// multiplier, no jitter, infinite retries).
    pub reconnect: ReconnectConfig,
}

impl std::fmt::Debug for PushListenerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushListenerConfig")
            .field("ntfy_url", &"[redacted]")
            .field("topic", &"[redacted]")
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[redacted]"),
            )
            .field("reconnect", &self.reconnect)
            .finish_non_exhaustive()
    }
}

/// Only fixed labels or HTTP status codes can escape the transport boundary.
/// Arbitrary errors, remote SSE text and capability URLs never become logs.
#[derive(Debug)]
enum PushError {
    Endpoint,
    Trust,
    Transport,
    Stream,
    EventTooLarge,
    Status(u16),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint => f.write_str("ntfy endpoint rejected by transport policy"),
            Self::Trust => f.write_str("ntfy private-CA configuration unavailable"),
            Self::Transport => f.write_str("ntfy transport failed"),
            Self::Stream => f.write_str("ntfy stream failed"),
            Self::EventTooLarge => f.write_str("ntfy event exceeded buffer limit"),
            Self::Status(status) => write!(f, "ntfy returned status {status}"),
        }
    }
}
impl std::error::Error for PushError {}

#[cfg(feature = "onion")]
struct AbortTask<T>(tokio::task::JoinHandle<T>);
#[cfg(feature = "onion")]
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type PushResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, PartialEq, Eq)]
enum PushTransport {
    PrivateHttps,
    Onion,
    DevLoopback,
}

/// Parse configuration once per attempt, before opening any socket. Topic is
/// one opaque path component; credentials/query/fragment are never accepted.
fn endpoint(config: &PushListenerConfig) -> Result<(url::Url, PushTransport), PushError> {
    let mut url = url::Url::parse(&config.ntfy_url).map_err(|_| PushError::Endpoint)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || config.topic.is_empty()
        || config.topic.len() > 128
        || !config
            .topic
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(PushError::Endpoint);
    }
    let host = url.host_str().ok_or(PushError::Endpoint)?;
    let transport = if host.ends_with(".onion") {
        let label = host.strip_suffix(".onion").ok_or(PushError::Endpoint)?;
        if label.len() != 56
            || !label
                .bytes()
                .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c))
            || !matches!(url.scheme(), "http" | "https")
        {
            return Err(PushError::Endpoint);
        }
        PushTransport::Onion
    } else if url.scheme() == "https" {
        PushTransport::PrivateHttps
    } else if cfg!(any(test, feature = "dev-trust-root"))
        && url.scheme() == "http"
        && matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        && url.port() == Some(8095)
    {
        // Explicit development ntfy port on literal loopback only. No LAN,
        // DNS alias, redirect or production cleartext exception.
        PushTransport::DevLoopback
    } else {
        return Err(PushError::Endpoint);
    };
    url.path_segments_mut()
        .map_err(|_| PushError::Endpoint)?
        .pop_if_empty()
        .push(&config.topic)
        .push("sse");
    Ok((url, transport))
}

fn direct_client(transport: PushTransport) -> Result<reqwest::Client, PushError> {
    match transport {
        PushTransport::PrivateHttps => {
            crate::network::storage_http::storage_client().map_err(|_| PushError::Trust)
        }
        PushTransport::DevLoopback => crate::network::public_http_client_builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| PushError::Transport),
        PushTransport::Onion => Err(PushError::Endpoint),
    }
}

impl PushListenerConfig {
    /// Create config for a pre-derived push topic.
    ///
    /// The topic is the opaque, server-issued value from AuthResponse — the
    /// client no longer derives `chatter_{user_id}` locally (that leaked the
    /// user_id and did not match the server's publish topic).
    pub fn new(ntfy_url: String, topic: String) -> Self {
        let reconnect = ReconnectConfig::const_default();
        Self {
            ntfy_url,
            force_tor: false,
            topic,
            auth_token: None,
            reconnect_delay: reconnect.initial_delay,
            reconnect,
        }
    }

    /// Set an auth token for topic subscription.
    pub fn with_auth(mut self, token: String) -> Self {
        self.auth_token = Some(token);
        self
    }

    /// Override the reconnect tuning. Also updates `reconnect_delay` to the
    /// supplied config's `initial_delay` for consistency.
    pub fn with_reconnect(mut self, reconnect: ReconnectConfig) -> Self {
        self.reconnect_delay = reconnect.initial_delay;
        self.reconnect = reconnect;
        self
    }
}

/// Ntfy push event received from the SSE stream.
#[derive(Debug, Clone)]
pub struct PushEvent {
    /// Event ID (from ntfy).
    pub id: String,
    /// Notification title.
    pub title: String,
    /// Notification body.
    pub message: String,
    /// Priority (1-5).
    pub priority: u8,
    /// Tags (emoji shortcodes).
    pub tags: Vec<String>,
}

impl PushEvent {
    /// Classify the privacy mode the server used when producing this event.
    ///
    /// Content-free mode is recognised by the canonical body string emitted by
    /// `internal/services/notification/push/ntfy.go` (`"New activity"`).
    /// Anything else implies the server is running in `full` mode.
    pub fn kind(&self) -> crate::inbox::handlers::PushKind {
        use crate::inbox::handlers::PushKind;
        if self.message == "New activity" {
            PushKind::ContentFree
        } else {
            PushKind::Full
        }
    }
}

/// Push listener that connects to a self-hosted ntfy instance.
///
/// On Android, this runs as a background service to receive push
/// notifications when the main gRPC stream is disconnected (app backgrounded).
///
/// On desktop, this is optional — the gRPC StreamNotifications RPC
/// is the primary notification channel.
pub struct PushListener {
    config: PushListenerConfig,
    #[cfg(feature = "onion")]
    tor: Option<crate::network::grpc::onion_transport::SharedTorClient>,
    /// Channel to notify when a push event arrives (wakes up stream manager).
    wake_tx: broadcast::Sender<PushEvent>,
}

impl PushListener {
    /// Broadcast channel capacity for push events.
    const WAKE_CAPACITY: usize = 16;

    /// Create a new push listener.
    pub fn new(config: PushListenerConfig) -> Self {
        let (wake_tx, _) = broadcast::channel(Self::WAKE_CAPACITY);
        Self {
            config,
            wake_tx,
            #[cfg(feature = "onion")]
            tor: None,
        }
    }

    /// Reuse the mailbox pool's bootstrapped substrate while isolating each
    /// subscription circuit. Avoid a second Tor bootstrap on mobile.
    #[cfg(feature = "onion")]
    pub(crate) fn with_tor(
        mut self,
        tor: crate::network::grpc::onion_transport::SharedTorClient,
    ) -> Self {
        self.tor = Some(tor);
        self
    }

    /// Subscribe to push wake-up events.
    ///
    /// Returns a broadcast receiver that fires when a push notification arrives.
    /// Consumers use this to reconnect gRPC streams / trigger an inbox poll.
    pub fn subscribe(&self) -> broadcast::Receiver<PushEvent> {
        self.wake_tx.subscribe()
    }

    /// Start listening for push events via SSE.
    ///
    /// This runs until the shutdown signal is received.
    /// On connection failure, it retries with exponential backoff.
    pub async fn listen(&self, mut shutdown: broadcast::Receiver<()>) {
        tracing::info!("Starting ntfy push listener");

        let reconnect_cfg = self.config.reconnect;
        let mut retry_delay = self.config.reconnect_delay;
        // `attempt` is 0 while we are connected; each failure increments it.
        let mut attempt: u32 = 0;
        // Wall-clock reference for the optional `give_up_after` deadline.
        let mut first_failure_at: Option<Instant> = None;

        loop {
            tokio::select! {
                // A queued Shield/sign-out shutdown wins before dialing.
                biased;
                _ = shutdown.recv() => {
                    tracing::debug!("Push listener shutting down");
                    return;
                }
                result = self.connect_sse() => {
                    match result {
                        Ok(()) => {
                            // Stream ended normally — reconnect immediately,
                            // reset backoff state.
                            tracing::info!("ntfy SSE stream ended, reconnecting...");
                            retry_delay = self.config.reconnect_delay;
                            attempt = 0;
                            first_failure_at = None;
                        }
                        Err(e) => {
                            if first_failure_at.is_none() {
                                first_failure_at = Some(Instant::now());
                            }
                            attempt = attempt.saturating_add(1);
                            // ntfy explicitly throttled this subscriber. A
                            // quick retry only extends the ban, especially
                            // when several devices share one public IP.
                            if matches!(
                                e.downcast_ref::<PushError>(),
                                Some(PushError::Status(429))
                            ) {
                                retry_delay = retry_delay.max(Duration::from_secs(60));
                            }
                            let elapsed = first_failure_at
                                .map(|t| t.elapsed())
                                .unwrap_or_default();

                            if reconnect_cfg.should_give_up(attempt, elapsed) {
                                tracing::error!(
                                    error = %e,
                                    attempt,
                                    elapsed_ms = elapsed.as_millis() as u64,
                                    "ntfy SSE gave up after reconnect budget exhausted"
                                );
                                return;
                            }

                            if self.config.force_tor {
                                // Anonymous mailbox SSE is an optional wake hint.
                                // The foreground inbox remains the delivery path.
                                tracing::debug!(
                                    error = %e,
                                    attempt,
                                    retry_in = ?retry_delay,
                                    "anonymous ntfy wake unavailable"
                                );
                            } else {
                                tracing::warn!(
                                    error = %e,
                                    attempt,
                                    retry_in = ?retry_delay,
                                    "ntfy SSE connection failed, retrying"
                                );
                            }
                        }
                    }
                }
            }

            // Apply optional jitter before sleeping. When `jitter_ratio == 0`
            // this is a no-op and the historical timing is preserved exactly.
            let sleep_for = if reconnect_cfg.jitter_ratio > 0.0 {
                use rand::Rng;
                let unit: f32 = rand::rng().random::<f32>();
                reconnect_cfg.with_jitter(retry_delay, unit)
            } else {
                retry_delay
            };

            // Wait before reconnecting (with shutdown check)
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::debug!("Push listener shutting down during reconnect wait");
                    return;
                }
                _ = tokio::time::sleep(sleep_for) => {}
            }

            // Advance backoff using the configured multiplier / cap.
            retry_delay = reconnect_cfg.next_delay(retry_delay);
        }
    }

    /// Connect to ntfy SSE endpoint and process events.
    async fn connect_sse(&self) -> PushResult {
        let (url, transport) = endpoint(&self.config)?;
        if transport == PushTransport::Onion || self.config.force_tor {
            #[cfg(feature = "onion")]
            {
                return self.connect_onion(&url).await;
            }
            #[cfg(not(feature = "onion"))]
            {
                return Err(PushError::Endpoint.into());
            }
        }
        let client = direct_client(transport)?;
        let mut request = client.get(url);
        if let Some(ref token) = self.config.auth_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|_| PushError::Transport)?;
        if !response.status().is_success() {
            return Err(PushError::Status(response.status().as_u16()).into());
        }
        tracing::debug!("Connected to ntfy SSE stream");
        use futures::StreamExt;
        let stream = response.bytes_stream().map(|r| {
            r.map_err(|_| Box::new(PushError::Stream) as Box<dyn std::error::Error + Send + Sync>)
        });
        self.dispatch_from_stream(stream).await
    }

    /// Anonymous subscriptions go through isolated Arti clients, never direct HTTP.
    /// Onion services use no exit; HTTPS endpoints keep the storage/gRPC private CA.
    #[cfg(feature = "onion")]
    async fn connect_onion(&self, url: &url::Url) -> PushResult {
        use crate::network::grpc::onion_transport::{
            SharedTorClient, tor_client_config, tor_runtime,
        };
        use std::sync::{Arc, OnceLock};
        static TOR: OnceLock<SharedTorClient> = OnceLock::new();
        let tor = self
            .tor
            .as_ref()
            .unwrap_or_else(|| TOR.get_or_init(|| Arc::new(tokio::sync::OnceCell::new())));
        let base = tor
            .get_or_try_init(|| async {
                let config = tor_client_config().map_err(|_| PushError::Transport)?;
                let mut task = AbortTask(tor_runtime().spawn(async move {
                    let _ =
                        tokio_rustls::rustls::crypto::ring::default_provider().install_default();
                    arti_client::TorClient::create_bootstrapped(config).await
                }));
                tokio::time::timeout(Duration::from_secs(120), &mut task.0)
                    .await
                    .map_err(|_| PushError::Transport)?
                    .map_err(|_| PushError::Transport)?
                    .map_err(|_| PushError::Transport)
            })
            .await?;
        let host = url.host_str().ok_or(PushError::Endpoint)?.to_owned();
        let port = url.port_or_known_default().ok_or(PushError::Endpoint)?;
        let client = base.isolated_client();
        let dial_host = host.clone();
        let mut dial = AbortTask(
            tor_runtime().spawn(async move { client.connect((dial_host.as_str(), port)).await }),
        );
        let stream = tokio::time::timeout(Duration::from_secs(90), &mut dial.0)
            .await
            .map_err(|_| PushError::Transport)?
            .map_err(|_| PushError::Transport)?
            .map_err(|_| PushError::Transport)?;
        if url.scheme() == "https" {
            use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
            use tokio_rustls::rustls::{ClientConfig, RootCertStore};
            let pem = crate::config::TLS_ROOT_CA_PEM.ok_or(PushError::Trust)?;
            let mut roots = RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
                roots
                    .add(cert.map_err(|_| PushError::Trust)?)
                    .map_err(|_| PushError::Trust)?;
            }
            if roots.is_empty() {
                return Err(PushError::Trust.into());
            }
            let config = ClientConfig::builder_with_provider(Arc::new(
                tokio_rustls::rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| PushError::Trust)?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let name = ServerName::try_from(host).map_err(|_| PushError::Endpoint)?;
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
            let tls =
                tokio::time::timeout(Duration::from_secs(15), connector.connect(name, stream))
                    .await
                    .map_err(|_| PushError::Transport)?
                    .map_err(|_| PushError::Transport)?;
            self.dispatch_onion(tls, url).await
        } else {
            self.dispatch_onion(stream, url).await
        }
    }

    #[cfg(feature = "onion")]
    async fn dispatch_onion<T>(&self, io: T, url: &url::Url) -> PushResult
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use futures::StreamExt;
        use http_body_util::{BodyExt, Full};
        let (mut sender, connection) = tokio::time::timeout(
            Duration::from_secs(15),
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io)),
        )
        .await
        .map_err(|_| PushError::Transport)?
        .map_err(|_| PushError::Transport)?;
        // Cancelling/reconnecting the listener must also stop its connection
        // driver. Its errors are deliberately not formatted into logs.
        let _driver = AbortTask(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let host = url.host_str().ok_or(PushError::Endpoint)?;
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let mut request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(url.path())
            .header(hyper::header::HOST, authority)
            .header(hyper::header::ACCEPT, "text/event-stream");
        if let Some(token) = &self.config.auth_token {
            let mut value = hyper::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| PushError::Endpoint)?;
            value.set_sensitive(true);
            request = request.header(hyper::header::AUTHORIZATION, value);
        }
        let request = request
            .body(Full::new(bytes::Bytes::new()))
            .map_err(|_| PushError::Endpoint)?;
        let response = tokio::time::timeout(Duration::from_secs(15), sender.send_request(request))
            .await
            .map_err(|_| PushError::Transport)?
            .map_err(|_| PushError::Transport)?;
        if !response.status().is_success() {
            return Err(PushError::Status(response.status().as_u16()).into());
        }
        // Hyper has no redirect-following machinery here: a 3xx is an error.
        let stream = response.into_body().into_data_stream().map(|r| {
            r.map_err(|_| Box::new(PushError::Stream) as Box<dyn std::error::Error + Send + Sync>)
        });
        self.dispatch_from_stream(stream).await
    }

    /// Consume an SSE byte-stream, parse events, and broadcast `PushEvent`s on
    /// the wake channel.
    ///
    /// Exposed at crate-visibility so unit tests can drive the dispatcher with
    /// a synthetic `Stream` (no network required). Returns `Ok(())` when the
    /// stream ends cleanly and `Err` on first transport failure.
    pub(crate) async fn dispatch_from_stream<S>(
        &self,
        mut stream: S,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>>
            + Unpin,
    {
        use futures::StreamExt;
        let mut lines = String::new();
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), stream.next())
            .await
            .map_err(|_| PushError::Stream)?
        {
            let chunk = chunk.map_err(|_| PushError::Stream)?;
            if lines.len().saturating_add(chunk.len()) > 128 * 1024 {
                return Err(PushError::EventTooLarge.into());
            }
            let text = String::from_utf8_lossy(&chunk);
            lines.push_str(&text);

            // Process complete SSE events (separated by double newline)
            while let Some(pos) = lines.find("\n\n") {
                let event_text = lines[..pos].to_string();
                lines = lines[pos + 2..].to_string();

                // ntfy occasionally emits `error: <text>\n\n` SSE lines (e.g.
                // subscription auth rejected). Log + ignore so the outer
                // reconnect loop can back off on its own cadence.
                if event_text.trim_start().starts_with("error:") {
                    tracing::warn!("ntfy SSE reported error");
                    continue;
                }

                if let Some(event) = self.parse_sse_event(&event_text) {
                    tracing::debug!("Push wake event received");

                    // Broadcast to wake up stream manager / show notification
                    let _ = self.wake_tx.send(event);
                }
            }
        }

        Ok(())
    }

    /// Parse an SSE event into a PushEvent.
    ///
    /// ntfy SSE format:
    /// ```text
    /// event: message
    /// data: {"id":"abc","title":"Chatter","message":"New activity","priority":3,"tags":["bell"]}
    /// ```
    fn parse_sse_event(&self, event_text: &str) -> Option<PushEvent> {
        let mut data_line = None;

        for line in event_text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                data_line = Some(data);
            }
        }

        let data = data_line?;

        // Parse JSON — ntfy sends JSON in data field
        let json: serde_json::Value = serde_json::from_str(data).ok()?;

        // Skip keepalive/open events
        let event_type = json.get("event")?.as_str()?;
        if event_type != "message" {
            return None;
        }

        Some(PushEvent {
            id: json.get("id")?.as_str().unwrap_or("").to_string(),
            title: json
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Chatter")
                .to_string(),
            message: json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("New activity")
                .to_string(),
            priority: json.get("priority").and_then(|v| v.as_u64()).unwrap_or(3) as u8,
            tags: json
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

// NOTE: `show_push_notification` was removed. ntfy push is now strictly
// a wake-up hint — the SSE title/body fields carry no structured data,
// so rendering a toast from them would fight the gRPC typed-notification
// stream which is the authoritative source for user-facing alerts
// (DeviceLinked, etc.). Keeping push toast-free also lets
// the server run ntfy in content-free mode without UI regression.
// If a platform needs a foreground-wake visual again, add a dedicated
// path that doesn't render push payload text verbatim.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_listener_config_new() {
        let config = PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "chatter_user-123".to_string(),
        );
        assert_eq!(config.topic, "chatter_user-123");
        assert_eq!(config.ntfy_url, "http://localhost:8090");
        assert!(config.auth_token.is_none());
    }

    #[test]
    fn test_push_listener_config_with_auth() {
        let config = PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "chatter_user-123".to_string(),
        )
        .with_auth("tk_test_token".to_string());
        assert_eq!(config.auth_token, Some("tk_test_token".to_string()));
    }

    #[test]
    fn test_parse_sse_event_valid() {
        let listener = PushListener::new(PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "test".to_string(),
        ));

        let event_text = r#"event: message
data: {"id":"abc123","event":"message","title":"Chatter","message":"New activity","priority":3,"tags":["bell"]}"#;

        let event = listener.parse_sse_event(event_text);
        assert!(event.is_some());

        let event = event.unwrap();
        assert_eq!(event.id, "abc123");
        assert_eq!(event.title, "Chatter");
        assert_eq!(event.message, "New activity");
        assert_eq!(event.priority, 3);
        assert_eq!(event.tags, vec!["bell"]);
    }

    #[test]
    fn test_parse_sse_event_keepalive_ignored() {
        let listener = PushListener::new(PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "test".to_string(),
        ));

        let event_text = r#"event: open
data: {"id":"abc","event":"open"}"#;

        let event = listener.parse_sse_event(event_text);
        assert!(event.is_none());
    }

    #[test]
    fn test_parse_sse_event_invalid_json() {
        let listener = PushListener::new(PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "test".to_string(),
        ));

        let event_text = "data: not valid json";
        let event = listener.parse_sse_event(event_text);
        assert!(event.is_none());
    }

    #[test]
    fn test_push_event_subscribe() {
        let listener = PushListener::new(PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "test".to_string(),
        ));

        let _rx1 = listener.subscribe();
        let _rx2 = listener.subscribe();
        // Multiple subscribers should work
    }

    #[test]
    fn test_push_listener_config_reconnect_delay() {
        let config = PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "chatter_user-123".to_string(),
        );
        assert_eq!(config.reconnect_delay, Duration::from_secs(5));
    }

    #[test]
    fn test_push_listener_config_default_reconnect_is_historical() {
        let config = PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "chatter_user-123".to_string(),
        );
        // Default config must preserve exact historical tuning so existing
        // deployments don't shift behavior after the refactor.
        assert_eq!(config.reconnect, ReconnectConfig::default());
        assert_eq!(config.reconnect.initial_delay, Duration::from_secs(5));
        assert_eq!(config.reconnect.max_delay, Duration::from_secs(60));
        assert_eq!(config.reconnect.backoff_multiplier, 2.0);
        assert_eq!(config.reconnect.max_attempts, None);
        assert_eq!(config.reconnect.give_up_after, None);
    }

    #[test]
    fn test_push_listener_config_with_reconnect_override() {
        let custom = ReconnectConfig {
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            backoff_multiplier: 1.5,
            jitter_ratio: 0.1,
            max_attempts: Some(3),
            give_up_after: Some(Duration::from_secs(60)),
        };
        let config = PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "chatter_user-123".to_string(),
        )
        .with_reconnect(custom);
        assert_eq!(config.reconnect, custom);
        // `reconnect_delay` should track the custom initial
        assert_eq!(config.reconnect_delay, Duration::from_millis(250));
    }

    #[test]
    fn test_push_event_kind_content_free() {
        use crate::inbox::handlers::PushKind;
        let ev = PushEvent {
            id: "a".into(),
            title: "Chatter".into(),
            message: "New activity".into(),
            priority: 3,
            tags: vec![],
        };
        assert_eq!(ev.kind(), PushKind::ContentFree);
    }

    #[test]
    fn test_push_event_kind_full() {
        use crate::inbox::handlers::PushKind;
        let ev = PushEvent {
            id: "a".into(),
            title: "Alice".into(),
            message: "hey are you free?".into(),
            priority: 3,
            tags: vec![],
        };
        assert_eq!(ev.kind(), PushKind::Full);
    }

    /// Feed a synthetic SSE stream (chunked bytes) into `dispatch_from_stream`
    /// and assert the parsed wake-ups land on the broadcast channel. No
    /// network involved — purely drives the byte-pipeline.
    #[tokio::test]
    async fn test_dispatch_from_stream_emits_wake_events() {
        use futures::stream;

        let listener = PushListener::new(PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "test".to_string(),
        ));
        let mut rx = listener.subscribe();

        // Two complete SSE events, split across three chunks to exercise
        // buffered-line handling across byte boundaries.
        let chunk_a = b"event: message\ndata: {\"id\":\"1\",\"event\":\"message\",\"title\":\"Chatter\",\"message\":\"New activity\",\"priority\":3,\"tags\":[]}\n\nevent: mes".to_vec();
        let chunk_b = b"sage\ndata: {\"id\":\"2\",\"event\":\"message\",\"title\":\"Alice\",\"message\":\"hi!\",\"priority\":3,\"tags\":[]}\n".to_vec();
        let chunk_c = b"\n".to_vec();

        let items: Vec<Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>> = vec![
            Ok(bytes::Bytes::from(chunk_a)),
            Ok(bytes::Bytes::from(chunk_b)),
            Ok(bytes::Bytes::from(chunk_c)),
        ];

        listener
            .dispatch_from_stream(stream::iter(items))
            .await
            .expect("stream should drain cleanly");

        let first = rx.try_recv().expect("first wake-up");
        assert_eq!(first.id, "1");
        assert_eq!(first.message, "New activity");

        let second = rx.try_recv().expect("second wake-up");
        assert_eq!(second.id, "2");
        assert_eq!(second.message, "hi!");

        assert!(rx.try_recv().is_err(), "no further events queued");
    }

    /// `error:` SSE lines from ntfy must be logged + skipped, not crash the
    /// dispatcher and not emit spurious wake-ups.
    #[tokio::test]
    async fn test_dispatch_from_stream_skips_error_lines() {
        use futures::stream;

        let listener = PushListener::new(PushListenerConfig::new(
            "http://localhost:8090".to_string(),
            "test".to_string(),
        ));
        let mut rx = listener.subscribe();

        let payload = b"error: forbidden topic\n\nevent: message\ndata: {\"id\":\"x\",\"event\":\"message\",\"title\":\"Chatter\",\"message\":\"New activity\",\"priority\":3,\"tags\":[]}\n\n".to_vec();

        let items: Vec<Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>> =
            vec![Ok(bytes::Bytes::from(payload))];

        listener
            .dispatch_from_stream(stream::iter(items))
            .await
            .expect("stream should drain cleanly");

        let ev = rx.try_recv().expect("wake-up after error line");
        assert_eq!(ev.id, "x");
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod transport_policy_tests {
    use super::*;

    fn config(url: &str) -> PushListenerConfig {
        PushListenerConfig::new(url.into(), "opaque-capability-topic".into())
            .with_auth("secret-bearer".into())
    }

    #[test]
    fn endpoint_rejects_credentials_queries_fragments_remote_cleartext_and_topic_paths() {
        for url in [
            "http://192.168.0.10:8095",
            "http://ntfy.example:8095",
            "http://127.0.0.1:80",
            "https://user:password@ntfy.example",
            "https://ntfy.example/?token=secret",
            "https://ntfy.example/#secret",
            "ftp://ntfy.example",
            "http://fake.onion",
            "http://127.0.0.1.evil:8095",
        ] {
            assert!(endpoint(&config(url)).is_err());
        }
        for topic in ["../secret", "a/b", "a?secret", "a#secret", "", "a%2fb"] {
            let mut cfg = config("https://ntfy.example");
            cfg.topic = topic.into();
            assert!(endpoint(&cfg).is_err());
        }
    }

    #[test]
    fn endpoint_routes_private_https_and_v3_onion_without_direct_onion_fallback() {
        let (url, route) = endpoint(&config("https://ntfy.example/base/")).unwrap();
        assert_eq!(route, PushTransport::PrivateHttps);
        assert_eq!(url.path(), "/base/opaque-capability-topic/sse");
        let onion = format!("http://{}.onion:8095", "a".repeat(56));
        assert_eq!(endpoint(&config(&onion)).unwrap().1, PushTransport::Onion);
        let loopback = endpoint(&config("http://127.0.0.1:8095")).unwrap();
        assert_eq!(loopback.1, PushTransport::DevLoopback);
        assert!(direct_client(PushTransport::Onion).is_err());
    }

    #[test]
    fn config_debug_never_discloses_topic_auth_or_full_url() {
        let cfg = config("https://ntfy.example/private-capability");
        let rendered = format!("{cfg:?}");
        for secret in [
            "ntfy.example",
            "private-capability",
            "opaque-capability-topic",
            "secret-bearer",
        ] {
            assert!(!rendered.contains(secret));
        }
    }

    #[tokio::test]
    async fn stream_errors_are_sanitized_and_do_not_emit_wakes() {
        let listener = PushListener::new(config("https://ntfy.example"));
        let mut receiver = listener.subscribe();
        let error: Box<dyn std::error::Error + Send + Sync> =
            "failed https://host/opaque-capability-topic?secret-bearer".into();
        let stream = futures::stream::iter(vec![Err(error)]);
        let error = listener.dispatch_from_stream(stream).await.unwrap_err();
        assert_eq!(error.to_string(), "ntfy stream failed");
        assert_eq!(format!("{error:?}"), "Stream");
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn oversized_unterminated_sse_is_bounded() {
        let listener = PushListener::new(config("https://ntfy.example"));
        let stream =
            futures::stream::iter(vec![Ok(bytes::Bytes::from(vec![b'x'; 128 * 1024 + 1]))]);
        assert_eq!(
            listener
                .dispatch_from_stream(stream)
                .await
                .unwrap_err()
                .to_string(),
            "ntfy event exceeded buffer limit"
        );
    }

    #[tokio::test]
    async fn private_https_transport_rejects_cleartext_before_contact() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = direct_client(PushTransport::PrivateHttps).unwrap();
        assert!(
            client
                .get(format!(
                    "http://{}/opaque-capability-topic/sse",
                    target.local_addr().unwrap()
                ))
                .send()
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn onion_http_dispatch_does_not_follow_redirect_or_leak_its_target() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = PushListener::new(config(&format!("http://{}.onion", "a".repeat(56))));
        let (url, _) = endpoint(&listener.config).unwrap();
        let (client, mut server) = tokio::io::duplex(4096);
        let serving = tokio::spawn(async move {
            let mut request = [0u8; 4096];
            let count = server.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("GET /opaque-capability-topic/sse HTTP/1.1"));
            server.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://stolen-topic.example/secret-bearer\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        });
        let error = listener.dispatch_onion(client, &url).await.unwrap_err();
        assert_eq!(error.to_string(), "ntfy returned status 302");
        assert!(!format!("{error:?}").contains("secret-bearer"));
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn direct_transport_never_follows_capability_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = source.local_addr().unwrap();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/stolen-capability\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(redirect.as_bytes()).await.unwrap();
        });
        // Exercise the same builder used after explicit development-loopback
        // validation, on an ephemeral test port rather than the deployed port.
        let response = direct_client(PushTransport::DevLoopback)
            .unwrap()
            .get(format!("http://{address}/opaque-capability-topic/sse"))
            .bearer_auth("secret-bearer")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
mod log_redaction_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn server_error_text_event_payload_and_url_do_not_reach_logs() {
        const ISOLATED_CAPTURE: &str = "CHATTER_PUSH_LOG_REDACTION_CHILD";
        if std::env::var_os(ISOLATED_CAPTURE).is_none() {
            // Callsite interest and the maximum enabled level are process-global.
            // In tracing-core's single-dispatch fast path, another test thread
            // registering this callsite can cache its own default subscriber's
            // "never" interest, even while our capture subscriber is alive.
            // Run the real assertions alone in this same test executable rather
            // than retrying emissions or racing another cache rebuild.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "network::streaming::push_listener::log_redaction_tests::server_error_text_event_payload_and_url_do_not_reach_logs",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(ISOLATED_CAPTURE, "1")
                .output()
                .expect("launch isolated push log redaction test");
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "isolated push log redaction test failed or did not run: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        let listener = PushListener::new(
            PushListenerConfig::new(
                "https://private-host/secret-url".into(),
                "opaque-capability-topic".into(),
            )
            .with_auth("secret-bearer".into()),
        );
        let mut wakes = listener.subscribe();
        let payload = b"error: https://private-host/secret-url/opaque-capability-topic secret-bearer\n\nevent: message\ndata: {\"id\":\"private-event-id\",\"event\":\"message\",\"title\":\"private-title\",\"message\":\"New activity\"}\n\n";
        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from_static(payload))]);
        listener
            .dispatch_from_stream(stream)
            .with_subscriber(subscriber)
            .await
            .unwrap();
        assert_eq!(
            wakes.try_recv().unwrap().kind(),
            crate::inbox::handlers::PushKind::ContentFree
        );
        let captured = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(captured.contains("ntfy SSE reported error"));
        assert!(captured.contains("Push wake event received"));
        for secret in [
            "private-host",
            "secret-url",
            "opaque-capability-topic",
            "secret-bearer",
            "private-title",
            "private-event-id",
        ] {
            assert!(!captured.contains(secret));
        }
    }
}

#[cfg(test)]
mod shutdown_admission_tests {
    #[tokio::test]
    async fn queued_shutdown_prevents_opening_a_socket() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = super::PushListenerConfig::new(
            format!("https://127.0.0.1:{}", server.local_addr().unwrap().port()),
            "fixture-topic".into(),
        );
        let listener = super::PushListener::new(config);
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        shutdown.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), listener.listen(rx))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), server.accept())
                .await
                .is_err()
        );
    }
}

#[cfg(all(test, not(feature = "onion")))]
mod mailbox_transport_tests {
    #[tokio::test]
    async fn anonymous_subscription_never_falls_back_to_direct() {
        let mut config = super::PushListenerConfig::new(
            "http://127.0.0.1:8095".into(),
            "chatter_0123456789abcdef0123456789abcdef".into(),
        );
        config.force_tor = true;
        let listener = super::PushListener::new(config);
        assert_eq!(
            listener.connect_sse().await.unwrap_err().to_string(),
            "ntfy endpoint rejected by transport policy"
        );
    }
}
