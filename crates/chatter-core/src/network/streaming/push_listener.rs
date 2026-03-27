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

use std::time::Duration;

use tokio::sync::broadcast;

#[cfg(not(target_os = "android"))]
use crate::system::notifications::DesktopNotifications;
use crate::domain::notification::NotificationPriority;

/// Configuration for the ntfy push listener.
#[derive(Clone, Debug)]
pub struct PushListenerConfig {
    /// Base URL of the self-hosted ntfy instance (e.g., "http://localhost:8090").
    pub ntfy_url: String,

    /// User's ntfy topic (e.g., "chatter_{user_id}").
    pub topic: String,

    /// Optional auth token for subscribing to the topic.
    pub auth_token: Option<String>,

    /// Reconnect delay on connection failure.
    pub reconnect_delay: Duration,
}

impl PushListenerConfig {
    /// Create config for a user's push topic.
    pub fn new(ntfy_url: String, user_id: &str) -> Self {
        Self {
            ntfy_url,
            topic: format!("chatter_{}", user_id),
            auth_token: None,
            reconnect_delay: Duration::from_secs(5),
        }
    }

    /// Set an auth token for topic subscription.
    pub fn with_auth(mut self, token: String) -> Self {
        self.auth_token = Some(token);
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

        let mut retry_delay = self.config.reconnect_delay;
        let max_delay = Duration::from_secs(60);

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::debug!("Push listener shutting down");
                    return;
                }
                result = self.connect_sse(&url) => {
                    match result {
                        Ok(()) => {
                            // Stream ended normally — reconnect immediately
                            tracing::info!("ntfy SSE stream ended, reconnecting...");
                            retry_delay = self.config.reconnect_delay;
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                retry_in = ?retry_delay,
                                "ntfy SSE connection failed, retrying"
                            );
                        }
                    }
                }
            }

            // Wait before reconnecting (with shutdown check)
            tokio::select! {
                _ = shutdown.recv() => {
                    tracing::debug!("Push listener shutting down during reconnect wait");
                    return;
                }
                _ = tokio::time::sleep(retry_delay) => {}
            }

            // Exponential backoff (capped)
            retry_delay = (retry_delay * 2).min(max_delay);
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

        // Reset retry delay on successful connection
        let mut lines = String::new();
        let mut stream = response.bytes_stream();

        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let text = String::from_utf8_lossy(&chunk);
            lines.push_str(&text);

            // Process complete SSE events (separated by double newline)
            while let Some(pos) = lines.find("\n\n") {
                let event_text = lines[..pos].to_string();
                lines = lines[pos + 2..].to_string();

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

/// Show a desktop notification from a push event.
///
/// Used when the push listener receives an event while the app is in the
/// foreground. On Android, this would trigger an Android notification instead.
#[cfg(not(target_os = "android"))]
pub fn show_push_notification(desktop: &DesktopNotifications, event: &PushEvent) {
    let priority = match event.priority {
        5 => NotificationPriority::Urgent,
        4 => NotificationPriority::High,
        2 => NotificationPriority::Low,
        _ => NotificationPriority::Normal,
    };

    if let Err(e) = desktop.show_message(&event.title, &event.message, priority) {
        tracing::warn!(error = %e, "Failed to show push notification");
    }
}

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
}
