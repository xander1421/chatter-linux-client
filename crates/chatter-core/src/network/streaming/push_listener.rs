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
//! Self-hosted ntfy ──[SSE]──> PushListener ──> wake StreamManager
//!                                           ──> show desktop notification
//! ```

use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::network::ReconnectConfig;

/// Configuration for the ntfy push listener.
#[derive(Clone, Debug)]
pub struct PushListenerConfig {
    /// Base URL of the self-hosted ntfy instance (e.g., "http://localhost:8090").
    pub ntfy_url: String,

    /// User's ntfy topic (e.g., "chatter_{user_id}").
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

impl PushListenerConfig {
    /// Create config for a user's push topic.
    pub fn new(ntfy_url: String, user_id: &str) -> Self {
        let reconnect = ReconnectConfig::const_default();
        Self {
            ntfy_url,
            topic: format!("chatter_{}", user_id),
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
    /// Channel to notify when a push event arrives (wakes up stream manager).
    wake_tx: broadcast::Sender<PushEvent>,
}

impl PushListener {
    /// Broadcast channel capacity for push events.
    const WAKE_CAPACITY: usize = 16;

    /// Create a new push listener.
    pub fn new(config: PushListenerConfig) -> Self {
        let (wake_tx, _) = broadcast::channel(Self::WAKE_CAPACITY);
        Self { config, wake_tx }
    }

    /// Subscribe to push wake-up events.
    ///
    /// Returns a broadcast receiver that fires when a push notification arrives.
    /// The StreamManager can use this to reconnect its gRPC streams.
    pub fn subscribe(&self) -> broadcast::Receiver<PushEvent> {
        self.wake_tx.subscribe()
    }

    /// Start listening for push events via SSE.
    ///
    /// This runs until the shutdown signal is received.
    /// On connection failure, it retries with exponential backoff.
    pub async fn listen(&self, mut shutdown: broadcast::Receiver<()>) {
        let url = format!(
            "{}/{}/sse",
            self.config.ntfy_url.trim_end_matches('/'),
            self.config.topic
        );

        tracing::info!(
            url = %url,
            topic = %self.config.topic,
            "Starting ntfy push listener"
        );

        let reconnect_cfg = self.config.reconnect;
        let mut retry_delay = self.config.reconnect_delay;
        // `attempt` is 0 while we are connected; each failure increments it.
        let mut attempt: u32 = 0;
        // Wall-clock reference for the optional `give_up_after` deadline.
        let mut first_failure_at: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::debug!("Push listener shutting down");
                    return;
                }
                result = self.connect_sse(&url) => {
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
    async fn connect_sse(&self, url: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let mut request = client.get(url);

        if let Some(ref token) = self.config.auth_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            return Err(format!("ntfy returned status {}", response.status()).into());
        }

        tracing::debug!("Connected to ntfy SSE stream");

        use futures::StreamExt;
        let stream = response.bytes_stream().map(|r| {
            r.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
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
        S: futures::Stream<
                Item = Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>,
            > + Unpin,
    {
        use futures::StreamExt;
        let mut lines = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
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
                    tracing::warn!(error = %event_text.trim(), "ntfy SSE reported error");
                    continue;
                }

                if let Some(event) = self.parse_sse_event(&event_text) {
                    tracing::debug!(
                        event_id = %event.id,
                        title = %event.title,
                        "Push event received"
                    );

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
            title: json.get("title").and_then(|v| v.as_str()).unwrap_or("Chatter").to_string(),
            message: json.get("message").and_then(|v| v.as_str()).unwrap_or("New activity").to_string(),
            priority: json.get("priority").and_then(|v| v.as_u64()).unwrap_or(3) as u8,
            tags: json.get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        })
    }
}

// NOTE: `show_push_notification` was removed. ntfy push is now strictly
// a wake-up hint — the SSE title/body fields carry no structured data,
// so rendering a toast from them would fight the gRPC typed-notification
// stream which is the authoritative source for user-facing alerts
// (DeviceLinked, IncomingCall, etc.). Keeping push toast-free also lets
// the server run ntfy in content-free mode without UI regression.
// If a platform needs a foreground-wake visual again, add a dedicated
// path that doesn't render push payload text verbatim.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_listener_config_new() {
        let config = PushListenerConfig::new("http://localhost:8090".to_string(), "user-123");
        assert_eq!(config.topic, "chatter_user-123");
        assert_eq!(config.ntfy_url, "http://localhost:8090");
        assert!(config.auth_token.is_none());
    }

    #[test]
    fn test_push_listener_config_with_auth() {
        let config = PushListenerConfig::new("http://localhost:8090".to_string(), "user-123")
            .with_auth("tk_test_token".to_string());
        assert_eq!(config.auth_token, Some("tk_test_token".to_string()));
    }

    #[test]
    fn test_parse_sse_event_valid() {
        let listener = PushListener::new(
            PushListenerConfig::new("http://localhost:8090".to_string(), "test"),
        );

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
        let listener = PushListener::new(
            PushListenerConfig::new("http://localhost:8090".to_string(), "test"),
        );

        let event_text = r#"event: open
data: {"id":"abc","event":"open"}"#;

        let event = listener.parse_sse_event(event_text);
        assert!(event.is_none());
    }

    #[test]
    fn test_parse_sse_event_invalid_json() {
        let listener = PushListener::new(
            PushListenerConfig::new("http://localhost:8090".to_string(), "test"),
        );

        let event_text = "data: not valid json";
        let event = listener.parse_sse_event(event_text);
        assert!(event.is_none());
    }

    #[test]
    fn test_push_event_subscribe() {
        let listener = PushListener::new(
            PushListenerConfig::new("http://localhost:8090".to_string(), "test"),
        );

        let _rx1 = listener.subscribe();
        let _rx2 = listener.subscribe();
        // Multiple subscribers should work
    }

    #[test]
    fn test_push_listener_config_reconnect_delay() {
        let config = PushListenerConfig::new("http://localhost:8090".to_string(), "user-123");
        assert_eq!(config.reconnect_delay, Duration::from_secs(5));
    }

    #[test]
    fn test_push_listener_config_default_reconnect_is_historical() {
        let config = PushListenerConfig::new("http://localhost:8090".to_string(), "user-123");
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
        let config = PushListenerConfig::new("http://localhost:8090".to_string(), "user-123")
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

        let listener = PushListener::new(
            PushListenerConfig::new("http://localhost:8090".to_string(), "test"),
        );
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

        let listener = PushListener::new(
            PushListenerConfig::new("http://localhost:8090".to_string(), "test"),
        );
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
