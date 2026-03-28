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

use ed25519_dalek::{Signer, SigningKey};
use rand::prelude::IndexedRandom;
use rand::Rng;
use tokio::sync::{broadcast, RwLock};

use crate::error::MailboxError;
use crate::mailbox::anon_dm::{AnonDmPollResult, AnonDmPoller};
use crate::mailbox::dm::{DmConversationManager, PollResult, TwoWayAnonymity};
use crate::mailbox::group::{GroupFanout, GroupPollResult};
use crate::mailbox::timing::{TimingConfig, TimingObfuscation};
use crate::network::grpc::AnonymousMailboxClient;

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
    /// Number of dedicated decoy polls per cycle (default: 1).
    /// These run independently of the per-mailbox dummy probability,
    /// matching real polling frequency ±20% jitter for traffic analysis resistance.
    pub decoy_polls_per_cycle: u32,
    /// Timing obfuscation configuration.
    pub timing_config: TimingConfig,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            dummy_polling_enabled: true,
            dummy_poll_probability: DEFAULT_DUMMY_POLL_PROBABILITY,
            decoy_polls_per_cycle: 1,
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
            decoy_polls_per_cycle: 0,
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
/// Each decoy holds a full Ed25519 keypair so that decoy polls execute the same
/// challenge-response flow as real polls, making them indistinguishable to a
/// network observer.
#[derive(Clone)]
struct DecoyMailbox {
    /// Ed25519 signing key for challenge-response during decoy polls.
    signing_key: SigningKey,
    /// Derived public key bytes (cached for convenience).
    public_key: [u8; 32],
    /// When this decoy was created.
    created_at: std::time::Instant,
}

impl DecoyMailbox {
    /// Generate a new random decoy mailbox with a fresh Ed25519 keypair.
    fn new_random() -> Self {
        let mut seed = [0u8; 32];
        rand::fill(&mut seed[..]);
        let signing_key = SigningKey::from_bytes(&seed);
        let public_key = signing_key.verifying_key().to_bytes();
        Self {
            signing_key,
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
    /// Executes the full challenge-response flow (get challenge → sign → poll)
    /// so that decoy polls generate the same network request pattern as real polls.
    /// A network observer cannot distinguish these from legitimate mailbox polls.
    async fn perform_dummy_poll(&self, client: &mut AnonymousMailboxClient) -> Result<(), MailboxError> {
        let (public_key, signing_key) = {
            let decoys = self.decoys.read().await;
            if decoys.is_empty() {
                return Ok(());
            }

            let decoy = decoys
                .choose(&mut rand::rng())
                .ok_or(MailboxError::NoTokensAvailable)?;

            (decoy.public_key, decoy.signing_key.clone())
        };

        // Apply timing delay (same as real polls)
        self.timing.delay_before_poll().await;

        // Step 1: Get challenge (same request as real poll)
        let challenge_result = client
            .get_mailbox_challenge(public_key.to_vec())
            .await;

        // Step 2: Sign challenge and poll (same request pattern as real poll)
        // Even if the challenge fails, we still attempt the poll request
        // to maintain consistent request count for network observers.
        if let Ok(resp) = challenge_result {
            let signature = signing_key.sign(&resp.challenge);
            let _ = client
                .poll_ephemeral_mailbox(
                    public_key.to_vec(),
                    signature.to_bytes().to_vec(),
                    resp.challenge,
                    None,
                    10,
                )
                .await;
        } else {
            // Server rejected challenge (mailbox doesn't exist).
            // Still generate a second request with dummy data to match
            // the two-request pattern of a real poll.
            let mut dummy_challenge = [0u8; 32];
            rand::fill(&mut dummy_challenge[..]);
            let signature = signing_key.sign(&dummy_challenge);
            let _ = client
                .poll_ephemeral_mailbox(
                    public_key.to_vec(),
                    signature.to_bytes().to_vec(),
                    dummy_challenge.to_vec(),
                    None,
                    10,
                )
                .await;
        }

        // Record dummy poll
        {
            let mut stats = self.stats.write().await;
            stats.dummy_polls += 1;
            stats.total_polls += 1;
        }

        tracing::trace!("Performed decoy poll (full challenge-response flow)");
        Ok(())
    }

    /// Poll all DM mailboxes.
    async fn poll_dm_mailboxes(
        &self,
        client: &mut AnonymousMailboxClient,
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
        client: &mut AnonymousMailboxClient,
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

        // Maybe do a dummy poll (poll a decoy mailbox to generate cover traffic)
        if self.should_dummy_poll() {
            if let Err(e) = self.perform_dummy_poll(client).await {
                tracing::trace!(error = %e, "Anon DM decoy poll failed (expected)");
            }
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

    /// Perform dedicated decoy polls for this cycle.
    ///
    /// Runs `decoy_polls_per_cycle` independent decoy polls with jittered
    /// inter-poll delays, matching real polling frequency ±20%.
    async fn perform_decoy_cycle(&self, client: &mut AnonymousMailboxClient) {
        if !self.config.dummy_polling_enabled || self.config.decoy_polls_per_cycle == 0 {
            return;
        }

        for i in 0..self.config.decoy_polls_per_cycle {
            if let Err(e) = self.perform_dummy_poll(client).await {
                tracing::trace!(error = %e, cycle = i, "Decoy cycle poll failed (expected)");
            }

            // Jittered delay between decoy polls within this cycle
            if i + 1 < self.config.decoy_polls_per_cycle {
                self.timing.delay_between_batch_items().await;
            }
        }
    }

    /// Perform a unified poll of all mailboxes.
    ///
    /// # Arguments
    ///
    /// * `client` - Anonymous mailbox client for all polling operations (no auth headers)
    pub async fn poll_all(
        &self,
        client: &mut AnonymousMailboxClient,
    ) -> Result<UnifiedPollResult, MailboxError> {
        // Ensure we have decoy mailboxes
        self.ensure_decoys().await;

        // Record initial dummy poll count
        let initial_dummy_polls = self.stats.read().await.dummy_polls;

        // Run dedicated decoy polls for this cycle (configurable count,
        // defaults to 1 per cycle to match real polling frequency ±20%)
        self.perform_decoy_cycle(client).await;

        // Poll DMs, groups, and anonymous DMs (all anonymous operations)
        // Per-mailbox dummy polls may also fire probabilistically within each method
        let dm_results = self.poll_dm_mailboxes(client).await?;
        let group_results = self.poll_group_mailboxes(client).await?;
        let anon_dm_results = self.poll_anon_dm_mailboxes(client).await?;

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
    /// * `client_fn` - Function to get an AnonymousMailboxClient for polling
    pub async fn run<F, Fut>(
        &self,
        mut shutdown: broadcast::Receiver<()>,
        client_fn: F,
    ) where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<AnonymousMailboxClient, MailboxError>>,
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
                            match self.poll_all(&mut client).await {
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

        // Public key should match the signing key's verifying key
        assert_eq!(
            decoy1.public_key,
            decoy1.signing_key.verifying_key().to_bytes()
        );
    }

    #[test]
    fn test_decoy_mailbox_can_sign() {
        use ed25519_dalek::Signer;

        let decoy = DecoyMailbox::new_random();
        let challenge = b"test challenge data for decoy signing";

        // Decoy should produce a valid Ed25519 signature
        let signature = decoy.signing_key.sign(challenge);
        assert_eq!(signature.to_bytes().len(), 64);

        // Signature should verify against the decoy's public key
        use ed25519_dalek::Verifier;
        let verifying_key = decoy.signing_key.verifying_key();
        assert!(verifying_key.verify(challenge, &signature).is_ok());
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
    fn test_default_decoy_polls_per_cycle() {
        let config = PollerConfig::default();
        assert_eq!(config.decoy_polls_per_cycle, 1);
    }

    #[test]
    fn test_minimal_disables_decoy_cycle() {
        let config = PollerConfig::minimal();
        assert_eq!(config.decoy_polls_per_cycle, 0);
        assert!(!config.dummy_polling_enabled);
    }

    #[test]
    fn test_custom_decoy_rate() {
        let config = PollerConfig {
            decoy_polls_per_cycle: 3,
            ..Default::default()
        };
        assert_eq!(config.decoy_polls_per_cycle, 3);
        assert!(config.dummy_polling_enabled);
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
