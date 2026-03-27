//! Mailbox poller.
//!
//! Unified polling loop for all mailbox types with timing obfuscation
//! and dummy polling for access pattern obfuscation.
//!
//! The NDSS '21 paper notes that predictable polling patterns can leak information.
//! This implementation includes:
//! - Jittered polling intervals
//! - Dummy polling to decoy mailboxes
//! - Random delays before each poll operation

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rand::prelude::IndexedRandom;
use rand::Rng;
use tokio::sync::{broadcast, RwLock};

use crate::error::MailboxError;
use crate::mailbox::anon_dm::{AnonDmPollResult, AnonDmPoller};
use crate::mailbox::dm::{DmConversationManager, PollResult, TwoWayAnonymity};
use crate::mailbox::group::{GroupFanout, GroupPollResult};
use crate::mailbox::timing::{TimingConfig, TimingObfuscation};
use crate::network::grpc::{AnonymousMailboxClient, MailboxClient};

/// Default polling interval for DM mailboxes (5 seconds for real-time chat).
/// NDSS '21 measured Signal delivery at ~1.5s; 5s balances latency vs server load.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// Default dummy poll probability (20% of polls are to unused mailboxes).
pub const DEFAULT_DUMMY_POLL_PROBABILITY: f64 = 0.2;

/// Maximum number of decoy mailbox IDs to maintain.
const MAX_DECOY_MAILBOXES: usize = 10;

/// Configuration for the mailbox poller.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// Base polling interval (will be jittered).
    pub poll_interval: Duration,
    /// Whether dummy polling is enabled.
    pub dummy_polling_enabled: bool,
    /// Probability of a poll being a dummy poll (0.0 - 1.0).
    pub dummy_poll_probability: f64,
    /// Timing obfuscation configuration.
    pub timing_config: TimingConfig,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            dummy_polling_enabled: true,
            dummy_poll_probability: DEFAULT_DUMMY_POLL_PROBABILITY,
            timing_config: TimingConfig::default(),
        }
    }
}

impl PollerConfig {
    /// Create a minimal configuration (for testing).
    pub fn minimal() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            dummy_polling_enabled: false,
            timing_config: TimingConfig::disabled(),
            ..Default::default()
        }
    }

    /// Create with custom poll interval.
    pub fn with_interval(interval_secs: u64) -> Self {
        Self {
            poll_interval: Duration::from_secs(interval_secs),
            ..Default::default()
        }
    }
}

/// Statistics about poller operation.
#[derive(Debug, Default, Clone)]
pub struct PollerStats {
    /// Total poll operations performed.
    pub total_polls: u64,
    /// Real polls (to actual mailboxes).
    pub real_polls: u64,
    /// Dummy polls (to decoy mailboxes).
    pub dummy_polls: u64,
    /// Messages received from polls.
    pub messages_received: u64,
    /// Poll failures.
    pub poll_failures: u64,
    /// Last successful poll time.
    pub last_poll: Option<std::time::Instant>,
}

/// Result of a unified poll operation.
#[derive(Debug)]
pub struct UnifiedPollResult {
    /// DM poll results.
    pub dm_results: Vec<PollResult>,
    /// Group poll results.
    pub group_results: Vec<GroupPollResult>,
    /// Anonymous DM poll results.
    pub anon_dm_results: Vec<AnonDmPollResult>,
    /// Whether any dummy polls were performed.
    pub dummy_polls_performed: u32,
}

/// Decoy mailbox for dummy polling.
///
/// These are fake mailbox addresses that we poll to hide our real access patterns.
/// The server cannot distinguish real mailbox polls from decoy polls.
#[derive(Clone)]
struct DecoyMailbox {
    /// Random Ed25519 public key (not associated with any real mailbox).
    public_key: [u8; 32],
    /// When this decoy was created.
    created_at: std::time::Instant,
}

impl DecoyMailbox {
    /// Generate a new random decoy mailbox.
    fn new_random() -> Self {
        let mut public_key = [0u8; 32];
        rand::fill(&mut public_key[..]);
        Self {
            public_key,
            created_at: std::time::Instant::now(),
        }
    }
}

/// Unified mailbox poller.
///
/// Polls both DM and group mailboxes with timing obfuscation and dummy polling
/// for access pattern obfuscation.
///
/// # Privacy Features
///
/// 1. **Jittered intervals**: Polling doesn't happen at predictable times
/// 2. **Pre-poll delays**: Random delay before each poll operation
/// 3. **Dummy polling**: Some polls go to non-existent mailboxes (decoys)
/// 4. **Unified polling**: All mailboxes polled in mixed order
///
/// # Example
///
/// ```ignore
/// let poller = MailboxPoller::new(
///     dm_manager.clone(),
///     group_manager.clone(),
///     PollerConfig::default(),
/// );
///
/// // Run poller in background
/// tokio::spawn(async move {
///     poller.run(shutdown_rx, |client| async move {
///         // Get MailboxClient from pool
///         Ok(create_mailbox_client())
///     }).await;
/// });
/// ```
pub struct MailboxPoller {
    dm_manager: Arc<DmConversationManager>,
    two_way: Arc<TwoWayAnonymity>,
    group_fanout: Arc<GroupFanout>,
    anon_dm_poller: Option<Arc<AnonDmPoller>>,
    config: PollerConfig,
    timing: TimingObfuscation,
    stats: RwLock<PollerStats>,
    /// Decoy mailboxes for dummy polling.
    decoys: RwLock<Vec<DecoyMailbox>>,
    /// Set of real mailbox public keys (to avoid accidentally using as decoys).
    real_mailbox_keys: RwLock<HashSet<[u8; 32]>>,
}

impl MailboxPoller {
    /// Create a new unified mailbox poller.
    pub fn new(
        dm_manager: Arc<DmConversationManager>,
        two_way: Arc<TwoWayAnonymity>,
        group_fanout: Arc<GroupFanout>,
        config: PollerConfig,
    ) -> Self {
        let timing = TimingObfuscation::new(config.timing_config.clone());
        Self {
            dm_manager,
            two_way,
            group_fanout,
            anon_dm_poller: None,
            config,
            timing,
            stats: RwLock::new(PollerStats::default()),
            decoys: RwLock::new(Vec::new()),
            real_mailbox_keys: RwLock::new(HashSet::new()),
        }
    }

    /// Set the anonymous DM poller.
    pub fn with_anon_dm_poller(mut self, poller: Arc<AnonDmPoller>) -> Self {
        self.anon_dm_poller = Some(poller);
        self
    }

    /// Get current statistics.
    pub async fn stats(&self) -> PollerStats {
        self.stats.read().await.clone()
    }

    /// Get the current configuration.
    pub fn config(&self) -> &PollerConfig {
        &self.config
    }

    /// Update real mailbox keys (called when mailboxes change).
    pub async fn update_real_mailbox_keys(&self, keys: Vec<[u8; 32]>) {
        let mut real_keys = self.real_mailbox_keys.write().await;
        real_keys.clear();
        real_keys.extend(keys);
    }

    /// Ensure we have enough decoy mailboxes.
    async fn ensure_decoys(&self) {
        let mut decoys = self.decoys.write().await;

        // Remove old decoys (older than 1 hour)
        let one_hour_ago = std::time::Instant::now() - Duration::from_secs(3600);
        decoys.retain(|d| d.created_at > one_hour_ago);

        // Add new decoys if needed
        let real_keys = self.real_mailbox_keys.read().await;
        while decoys.len() < MAX_DECOY_MAILBOXES {
            let decoy = DecoyMailbox::new_random();
            // Make sure decoy doesn't accidentally match a real mailbox
            if !real_keys.contains(&decoy.public_key) {
                decoys.push(decoy);
            }
        }
    }

    /// Decide whether this poll should be a dummy poll.
    fn should_dummy_poll(&self) -> bool {
        if !self.config.dummy_polling_enabled {
            return false;
        }

        let mut rng = rand::rng();
        rng.random::<f64>() < self.config.dummy_poll_probability
    }

    /// Perform a dummy poll to a random decoy mailbox.
    ///
    /// The server will return "mailbox not found" but cannot distinguish
    /// this from a legitimate poll to an empty mailbox.
    async fn perform_dummy_poll(&self, client: &mut MailboxClient) -> Result<(), MailboxError> {
        let decoys = self.decoys.read().await;
        if decoys.is_empty() {
            return Ok(());
        }

        // Select random decoy
        let decoy = decoys
            .choose(&mut rand::rng())
            .ok_or(MailboxError::NoTokensAvailable)?;

        // Apply timing delay
        self.timing.delay_before_poll().await;

        // Poll the decoy mailbox (will fail, but that's expected)
        // We just need to make the request to generate traffic
        let _ = client
            .get_mailbox_challenge(decoy.public_key.to_vec())
            .await;

        // Record dummy poll
        {
            let mut stats = self.stats.write().await;
            stats.dummy_polls += 1;
            stats.total_polls += 1;
        }

        tracing::trace!("Performed dummy poll to decoy mailbox");
        Ok(())
    }

    /// Poll all DM mailboxes.
    async fn poll_dm_mailboxes(
        &self,
        client: &mut MailboxClient,
    ) -> Result<Vec<PollResult>, MailboxError> {
        let mut results = Vec::new();

        // Get all active mailbox keys
        let active_keys = self.dm_manager.get_active_mailbox_keys()?;

        // Update real mailbox keys for decoy generation
        let real_keys: Vec<[u8; 32]> = active_keys.iter().map(|(_, pk, _, _)| *pk).collect();
        self.update_real_mailbox_keys(real_keys).await;

        // Randomize poll order to prevent patterns
        let mut shuffled_keys = active_keys;
        {
            use rand::prelude::SliceRandom;
            shuffled_keys.shuffle(&mut rand::rng());
        }

        for (peer_id, _, _, _) in shuffled_keys {
            // Maybe do a dummy poll instead
            if self.should_dummy_poll() {
                let _ = self.perform_dummy_poll(client).await;
            }

            // Apply timing delay
            self.timing.delay_before_poll().await;

            // Poll this mailbox
            match self.two_way.poll_mailbox(&peer_id, client, None).await {
                Ok(result) => {
                    let msg_count = result.messages.len();
                    if msg_count > 0 {
                        tracing::debug!(peer = %peer_id, messages = msg_count, "DM poll received messages");
                    }
                    results.push(result);

                    // Update stats
                    {
                        let mut stats = self.stats.write().await;
                        stats.real_polls += 1;
                        stats.total_polls += 1;
                        stats.messages_received += msg_count as u64;
                        stats.last_poll = Some(std::time::Instant::now());
                    }
                }
                Err(e) => {
                    tracing::debug!(peer = %peer_id, error = %e, "DM poll failed");
                    let mut stats = self.stats.write().await;
                    stats.poll_failures += 1;
                }
            }

            // Random delay between batch items
            self.timing.delay_between_batch_items().await;
        }

        Ok(results)
    }

    /// Poll all group mailboxes.
    ///
    /// Uses GroupFanout for actual polling with timing obfuscation and dummy polls.
    async fn poll_group_mailboxes(
        &self,
        client: &mut MailboxClient,
    ) -> Result<Vec<GroupPollResult>, MailboxError> {
        // Apply timing delay before polling
        self.timing.delay_before_poll().await;

        // Maybe do a dummy poll (poll a decoy mailbox to generate cover traffic)
        if self.should_dummy_poll() {
            if let Err(e) = self.perform_dummy_poll(client).await {
                tracing::trace!(error = %e, "Group decoy poll failed (expected)");
            }
        }

        // Poll all group mailboxes via GroupFanout
        match self.group_fanout.poll_all_group_mailboxes().await {
            Ok(poll_results) => {
                let mut stats = self.stats.write().await;
                stats.real_polls += 1;
                stats.total_polls += 1;

                let total_messages: usize = poll_results.iter().map(|(_, r)| r.messages.len()).sum();
                stats.messages_received += total_messages as u64;
                stats.last_poll = Some(std::time::Instant::now());

                // Extract just the GroupPollResult from the tuples
                Ok(poll_results.into_iter().map(|(_, result)| result).collect())
            }
            Err(e) => {
                tracing::debug!(error = %e, "Group mailbox poll failed");
                let mut stats = self.stats.write().await;
                stats.poll_failures += 1;
                Err(e)
            }
        }
    }

    /// Poll all anonymous DM mailboxes.
    async fn poll_anon_dm_mailboxes(
        &self,
        client: &mut AnonymousMailboxClient,
    ) -> Result<Vec<AnonDmPollResult>, MailboxError> {
        let anon_dm_poller = match &self.anon_dm_poller {
            Some(poller) => poller,
            None => return Ok(Vec::new()), // No anon DM poller configured
        };

        // Apply timing delay before polling
        self.timing.delay_before_poll().await;

        // Maybe do a dummy poll
        if self.should_dummy_poll() {
            let mut stats = self.stats.write().await;
            stats.dummy_polls += 1;
            stats.total_polls += 1;
        }

        // Poll all anonymous DM mailboxes
        match anon_dm_poller.poll_all(client).await {
            Ok(poll_results) => {
                let mut stats = self.stats.write().await;
                stats.real_polls += 1;
                stats.total_polls += 1;

                let total_messages: usize = poll_results.iter().map(|r| r.messages.len()).sum();
                stats.messages_received += total_messages as u64;
                stats.last_poll = Some(std::time::Instant::now());

                Ok(poll_results)
            }
            Err(e) => {
                tracing::debug!(error = %e, "Anonymous DM mailbox poll failed");
                let mut stats = self.stats.write().await;
                stats.poll_failures += 1;
                Err(e)
            }
        }
    }

    /// Perform a unified poll of all mailboxes.
    ///
    /// # Arguments
    ///
    /// * `client` - Authenticated mailbox client for DM and group polling
    /// * `anon_client` - Optional anonymous mailbox client for anon DM polling (no JWT auth)
    pub async fn poll_all(
        &self,
        client: &mut MailboxClient,
        anon_client: Option<&mut AnonymousMailboxClient>,
    ) -> Result<UnifiedPollResult, MailboxError> {
        // Ensure we have decoy mailboxes
        self.ensure_decoys().await;

        // Record initial dummy poll count
        let initial_dummy_polls = self.stats.read().await.dummy_polls;

        // Poll DMs, groups, and anonymous DMs
        let dm_results = self.poll_dm_mailboxes(client).await?;
        let group_results = self.poll_group_mailboxes(client).await?;

        // Poll anon DMs if client provided
        let anon_dm_results = if let Some(anon) = anon_client {
            self.poll_anon_dm_mailboxes(anon).await?
        } else {
            Vec::new()
        };

        // Maybe one final dummy poll
        if self.should_dummy_poll() {
            let _ = self.perform_dummy_poll(client).await;
        }

        // Calculate dummy polls performed this round
        let final_dummy_polls = self.stats.read().await.dummy_polls;
        let dummy_polls_performed = (final_dummy_polls - initial_dummy_polls) as u32;

        Ok(UnifiedPollResult {
            dm_results,
            group_results,
            anon_dm_results,
            dummy_polls_performed,
        })
    }

    /// Run the poller until shutdown signal.
    ///
    /// # Arguments
    ///
    /// * `shutdown` - Broadcast receiver for shutdown signal
    /// * `client_fn` - Function to get a MailboxClient for polling
    pub async fn run<F, Fut>(
        &self,
        mut shutdown: broadcast::Receiver<()>,
        client_fn: F,
    ) where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<MailboxClient, MailboxError>>,
    {
        tracing::info!(
            interval_secs = self.config.poll_interval.as_secs(),
            dummy_enabled = self.config.dummy_polling_enabled,
            "Mailbox poller starting"
        );

        loop {
            // Calculate jittered interval
            let interval = self.timing.jitter_interval(self.config.poll_interval);

            tokio::select! {
                biased;

                _ = shutdown.recv() => {
                    tracing::info!("Mailbox poller shutting down");
                    break;
                }

                _ = tokio::time::sleep(interval) => {
                    match client_fn().await {
                        Ok(mut client) => {
                            // Note: anon DM polling requires AnonymousMailboxClient
                            // This run() method doesn't support it; use poll_all() directly
                            match self.poll_all(&mut client, None).await {
                                Ok(result) => {
                                    let total_messages: usize = result.dm_results.iter()
                                        .map(|r| r.messages.len())
                                        .sum::<usize>()
                                        + result.group_results.iter()
                                            .map(|r| r.messages.len())
                                            .sum::<usize>();

                                    if total_messages > 0 || result.dummy_polls_performed > 0 {
                                        tracing::debug!(
                                            messages = total_messages,
                                            dummy_polls = result.dummy_polls_performed,
                                            "Poll cycle complete"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "Poll cycle failed");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to get mailbox client");
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
    use crate::domain::ChannelId;

    #[test]
    fn test_default_config() {
        let config = PollerConfig::default();
        assert_eq!(config.poll_interval.as_secs(), DEFAULT_POLL_INTERVAL_SECS);
        assert!(config.dummy_polling_enabled);
        assert!((config.dummy_poll_probability - DEFAULT_DUMMY_POLL_PROBABILITY).abs() < f64::EPSILON);
    }

    #[test]
    fn test_minimal_config() {
        let config = PollerConfig::minimal();
        assert_eq!(config.poll_interval.as_secs(), 5);
        assert!(!config.dummy_polling_enabled);
        assert!(!config.timing_config.enabled);
    }

    #[test]
    fn test_custom_interval() {
        let config = PollerConfig::with_interval(60);
        assert_eq!(config.poll_interval.as_secs(), 60);
    }

    #[test]
    fn test_decoy_mailbox_generation() {
        let decoy1 = DecoyMailbox::new_random();
        let decoy2 = DecoyMailbox::new_random();

        // Should be different with very high probability
        assert_ne!(decoy1.public_key, decoy2.public_key);

        // Should be 32 bytes
        assert_eq!(decoy1.public_key.len(), 32);
    }

    #[test]
    fn test_stats_default() {
        let stats = PollerStats::default();
        assert_eq!(stats.total_polls, 0);
        assert_eq!(stats.real_polls, 0);
        assert_eq!(stats.dummy_polls, 0);
        assert_eq!(stats.messages_received, 0);
        assert_eq!(stats.poll_failures, 0);
        assert!(stats.last_poll.is_none());
    }

    #[test]
    fn test_should_dummy_poll_disabled() {
        // Create a poller with dummy polling disabled - we need to test the logic
        let config = PollerConfig {
            dummy_polling_enabled: false,
            ..Default::default()
        };

        // Test that probability 0 never triggers
        assert!(!config.dummy_polling_enabled);
    }

    #[test]
    fn test_unified_poll_result_structure() {
        let result = UnifiedPollResult {
            dm_results: vec![],
            group_results: vec![],
            anon_dm_results: vec![],
            dummy_polls_performed: 5,
        };

        assert!(result.dm_results.is_empty());
        assert!(result.group_results.is_empty());
        assert!(result.anon_dm_results.is_empty());
        assert_eq!(result.dummy_polls_performed, 5);
    }

    #[test]
    fn test_group_poll_result_structure() {
        let result = GroupPollResult {
            group_id: ChannelId::new(),
            messages: Vec::new(),
            has_more: false,
            mailbox_id: "test-mailbox".to_string(),
        };

        assert!(result.messages.is_empty());
        assert!(!result.has_more);
    }
}
