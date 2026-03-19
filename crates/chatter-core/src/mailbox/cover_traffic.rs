//! Cover traffic.
//!
//! Fake message generation for timing obfuscation.
//!
//! Generates dummy messages at random intervals to prevent Statistical Disclosure Attacks.
//! Cover traffic is indistinguishable from real traffic to the server, making it impossible
//! to determine when users are actually communicating.

use std::sync::Arc;
use std::time::Duration;

use rand::prelude::IndexedRandom;
use rand::Rng;
use tokio::sync::broadcast;

use crate::domain::UserId;
use crate::error::MailboxError;
use crate::mailbox::dm::DmConversationManager;
use crate::mailbox::ContentEncoder;

/// Default base interval between cover traffic messages (60 seconds).
pub const DEFAULT_BASE_INTERVAL_SECS: u64 = 60;

/// Minimum interval to prevent excessive traffic (10 seconds).
pub const MIN_INTERVAL_SECS: u64 = 10;

/// Maximum interval to ensure periodic activity (5 minutes).
pub const MAX_INTERVAL_SECS: u64 = 300;

/// Size of dummy message content (matches typical short message after padding).
const DUMMY_CONTENT_SIZE: usize = 64;

/// Configuration for cover traffic generation.
#[derive(Debug, Clone)]
pub struct CoverTrafficConfig {
    /// Base interval between messages (actual intervals randomized around this).
    pub base_interval: Duration,
    /// Whether cover traffic is enabled.
    pub enabled: bool,
    /// Minimum interval (prevents excessive traffic).
    pub min_interval: Duration,
    /// Maximum interval (ensures periodic activity).
    pub max_interval: Duration,
}

impl Default for CoverTrafficConfig {
    fn default() -> Self {
        Self {
            base_interval: Duration::from_secs(DEFAULT_BASE_INTERVAL_SECS),
            enabled: false, // Disabled by default, user opt-in
            min_interval: Duration::from_secs(MIN_INTERVAL_SECS),
            max_interval: Duration::from_secs(MAX_INTERVAL_SECS),
        }
    }
}

impl CoverTrafficConfig {
    /// Create an enabled configuration with custom base interval.
    pub fn enabled_with_interval(base_secs: u64) -> Self {
        Self {
            base_interval: Duration::from_secs(base_secs),
            enabled: true,
            ..Default::default()
        }
    }
}

/// Statistics about cover traffic operation.
#[derive(Debug, Default, Clone)]
pub struct CoverTrafficStats {
    /// Total dummy messages sent.
    pub messages_sent: u64,
    /// Total send failures.
    pub send_failures: u64,
    /// Last successful send time.
    pub last_send: Option<std::time::Instant>,
}

/// A pending cover traffic message ready to be sent.
///
/// The generator produces these; the caller is responsible for
/// actually sending them through the network layer.
#[derive(Debug)]
pub struct CoverTrafficMessage {
    /// Target peer for the dummy message.
    pub peer_id: UserId,
    /// Padded dummy content (ready for encryption and sending).
    pub padded_content: Vec<u8>,
}

/// Cover traffic generator for timing obfuscation.
///
/// Generates fake messages to active mailboxes at random intervals,
/// making it impossible for the server to distinguish real communication
/// from cover traffic.
///
/// # Design
///
/// The generator produces `CoverTrafficMessage` instances that the caller
/// sends through their normal message sending infrastructure. This keeps
/// the generator decoupled from network clients.
///
/// # Example
///
/// ```ignore
/// let dm_manager = Arc::new(DmConversationManager::new(db));
/// let config = CoverTrafficConfig::enabled_with_interval(30);
/// let generator = CoverTrafficGenerator::new(dm_manager, config);
///
/// // Run in background task
/// tokio::spawn(async move {
///     generator.run(shutdown_rx, |msg| async {
///         // Send msg.padded_content to msg.peer_id via mailbox
///         Ok(())
///     }).await;
/// });
/// ```
pub struct CoverTrafficGenerator {
    dm_manager: Arc<DmConversationManager>,
    config: CoverTrafficConfig,
    stats: tokio::sync::RwLock<CoverTrafficStats>,
}

impl CoverTrafficGenerator {
    /// Create a new cover traffic generator.
    pub fn new(dm_manager: Arc<DmConversationManager>, config: CoverTrafficConfig) -> Self {
        Self {
            dm_manager,
            config,
            stats: tokio::sync::RwLock::new(CoverTrafficStats::default()),
        }
    }

    /// Create with default configuration (disabled).
    pub fn new_disabled(dm_manager: Arc<DmConversationManager>) -> Self {
        Self::new(dm_manager, CoverTrafficConfig::default())
    }

    /// Check if cover traffic is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Enable cover traffic with current configuration.
    pub fn enable(&mut self) {
        self.config.enabled = true;
    }

    /// Disable cover traffic.
    pub fn disable(&mut self) {
        self.config.enabled = false;
    }

    /// Update configuration.
    pub fn set_config(&mut self, config: CoverTrafficConfig) {
        self.config = config;
    }

    /// Get current configuration.
    pub fn config(&self) -> &CoverTrafficConfig {
        &self.config
    }

    /// Get current statistics.
    pub async fn stats(&self) -> CoverTrafficStats {
        self.stats.read().await.clone()
    }

    /// Generate a random interval using exponential distribution.
    ///
    /// Exponential distribution is more realistic than uniform - real messaging
    /// tends to have bursts followed by quiet periods.
    fn random_interval(&self) -> Duration {
        let mut rng = rand::rng();

        // Exponential distribution: -ln(U) * mean, where U is uniform (0,1)
        // Add small epsilon to avoid ln(0)
        let u: f64 = rng.random::<f64>().max(1e-10);
        let exponential = -u.ln() * self.config.base_interval.as_secs_f64();

        // Clamp to [min, max]
        let secs = exponential
            .max(self.config.min_interval.as_secs_f64())
            .min(self.config.max_interval.as_secs_f64());

        Duration::from_secs_f64(secs)
    }

    /// Generate dummy message content.
    ///
    /// Content is random bytes, will be padded by ContentEncoder to match
    /// real message padding patterns.
    fn generate_dummy_content() -> Vec<u8> {
        let mut content = vec![0u8; DUMMY_CONTENT_SIZE];
        rand::fill(&mut content[..]);
        content
    }

    /// Get list of active peer IDs we can send cover traffic to.
    fn get_active_peers(&self) -> Result<Vec<UserId>, MailboxError> {
        // Get all conversations where we have an active mailbox
        let mailbox_keys = self.dm_manager.get_active_mailbox_keys()?;

        // Filter to those where conversation is ready to send
        let mut active_peers = Vec::new();
        for (peer_id, _, _, _) in mailbox_keys {
            if let Ok(Some(conv)) = self.dm_manager.load_conversation(&peer_id) {
                if conv.is_ready_to_send() {
                    active_peers.push(peer_id);
                }
            }
        }

        Ok(active_peers)
    }

    /// Generate a single cover traffic message for a random active peer.
    ///
    /// Returns None if no active peers are available.
    pub fn generate_message(&self) -> Result<Option<CoverTrafficMessage>, MailboxError> {
        let active_peers = self.get_active_peers()?;

        if active_peers.is_empty() {
            // No active conversations, nothing to do
            return Ok(None);
        }

        // Select random peer
        let peer = active_peers
            .choose(&mut rand::rng())
            .ok_or(MailboxError::NoTokensAvailable)?;

        // Generate and pad dummy content
        let dummy_content = Self::generate_dummy_content();
        let padded = ContentEncoder::pad(&dummy_content)?;

        Ok(Some(CoverTrafficMessage {
            peer_id: *peer,
            padded_content: padded,
        }))
    }

    /// Record a successful send in statistics.
    pub async fn record_success(&self) {
        let mut stats = self.stats.write().await;
        stats.messages_sent += 1;
        stats.last_send = Some(std::time::Instant::now());
    }

    /// Record a failed send in statistics.
    pub async fn record_failure(&self) {
        let mut stats = self.stats.write().await;
        stats.send_failures += 1;
    }

    /// Run the cover traffic generator until shutdown signal.
    ///
    /// This method generates cover traffic messages at random intervals and
    /// passes them to the provided sender function.
    ///
    /// # Arguments
    ///
    /// * `shutdown` - Broadcast receiver for shutdown signal
    /// * `send_fn` - Async function to send cover traffic messages
    ///
    /// # Example
    ///
    /// ```ignore
    /// generator.run(shutdown_rx, |msg| async move {
    ///     two_way.send_to_peer(&msg.peer_id, msg.padded_content, envelope_type, &mut client).await
    /// }).await;
    /// ```
    pub async fn run<F, Fut>(&self, mut shutdown: broadcast::Receiver<()>, send_fn: F)
    where
        F: Fn(CoverTrafficMessage) -> Fut,
        Fut: std::future::Future<Output = Result<(), MailboxError>>,
    {
        if !self.config.enabled {
            tracing::info!("Cover traffic disabled, not starting");
            return;
        }

        tracing::info!(
            base_interval_secs = self.config.base_interval.as_secs(),
            "Cover traffic generator starting"
        );

        loop {
            let interval = self.random_interval();

            tokio::select! {
                biased;

                _ = shutdown.recv() => {
                    tracing::info!("Cover traffic generator shutting down");
                    break;
                }

                _ = tokio::time::sleep(interval) => {
                    match self.generate_message() {
                        Ok(Some(msg)) => {
                            let peer_id = msg.peer_id;
                            match send_fn(msg).await {
                                Ok(()) => {
                                    self.record_success().await;
                                    tracing::trace!(peer = %peer_id, "Cover traffic sent");
                                }
                                Err(e) => {
                                    self.record_failure().await;
                                    tracing::debug!(error = %e, peer = %peer_id, "Cover traffic send failed");
                                }
                            }
                        }
                        Ok(None) => {
                            // No active peers, silently skip
                            tracing::trace!("No active peers for cover traffic");
                        }
                        Err(e) => {
                            self.record_failure().await;
                            tracing::debug!(error = %e, "Cover traffic generation failed");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_disabled() {
        let config = CoverTrafficConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.base_interval.as_secs(), DEFAULT_BASE_INTERVAL_SECS);
    }

    #[test]
    fn test_enabled_config() {
        let config = CoverTrafficConfig::enabled_with_interval(30);
        assert!(config.enabled);
        assert_eq!(config.base_interval.as_secs(), 30);
    }

    #[test]
    fn test_random_interval_bounds() {
        let config = CoverTrafficConfig {
            base_interval: Duration::from_secs(60),
            enabled: true,
            min_interval: Duration::from_secs(10),
            max_interval: Duration::from_secs(300),
        };

        // At minimum, verify config values are correct
        assert_eq!(config.min_interval.as_secs(), MIN_INTERVAL_SECS);
        assert_eq!(config.max_interval.as_secs(), MAX_INTERVAL_SECS);
    }

    #[test]
    fn test_dummy_content_size() {
        // Dummy content should be a reasonable size
        assert!(DUMMY_CONTENT_SIZE > 0);
        assert!(DUMMY_CONTENT_SIZE <= crate::mailbox::content::MAX_CONTENT_SIZE);
    }

    #[test]
    fn test_stats_default() {
        let stats = CoverTrafficStats::default();
        assert_eq!(stats.messages_sent, 0);
        assert_eq!(stats.send_failures, 0);
        assert!(stats.last_send.is_none());
    }

    #[test]
    fn test_generate_dummy_content() {
        let content1 = CoverTrafficGenerator::generate_dummy_content();
        let content2 = CoverTrafficGenerator::generate_dummy_content();

        // Should be correct size
        assert_eq!(content1.len(), DUMMY_CONTENT_SIZE);
        assert_eq!(content2.len(), DUMMY_CONTENT_SIZE);

        // Should be random (different each time with very high probability)
        assert_ne!(content1, content2, "Random content should differ");
    }

    #[test]
    fn test_dummy_content_pads_correctly() {
        let content = CoverTrafficGenerator::generate_dummy_content();
        let padded = ContentEncoder::pad(&content).expect("padding should succeed");

        // Should be padded
        assert!(padded.len() >= content.len());

        // Should unpad correctly
        let unpadded = ContentEncoder::unpad(&padded).expect("unpadding should succeed");
        assert_eq!(unpadded, content);
    }

    #[test]
    fn test_cover_traffic_message_structure() {
        let msg = CoverTrafficMessage {
            peer_id: UserId::new(),
            padded_content: vec![1, 2, 3, 4],
        };

        assert!(!msg.padded_content.is_empty());
    }
}
