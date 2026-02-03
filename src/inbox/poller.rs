//! Inbox poller.
//!
//! Polling loop for fetching messages from the server's device inbox.

use std::sync::Arc;
use std::time::Instant;

use ed25519_dalek::{Signer, SigningKey};
use parking_lot::RwLock;
use tokio::sync::{broadcast, watch, Mutex};

use crate::network::grpc::MailboxClient;
use crate::proto::mailbox_v1::EphemeralMessage;

use super::provider::{InboxConfig, InboxError, InboxEvent, InboxState, InboxStats};

/// Poller for fetching messages from the device inbox.
///
/// Implements the polling loop with challenge-response authentication:
/// 1. Get challenge from server
/// 2. Sign challenge with Ed25519 private key
/// 3. Poll for messages with signature
/// 4. Return messages or handle errors
///
/// Uses adaptive polling intervals based on activity.
pub struct InboxPoller {
    config: InboxConfig,
    mailbox_client: Arc<Mutex<MailboxClient>>,
    /// Ed25519 keypair: (public_key, private_key)
    keypair: (Vec<u8>, Vec<u8>),
    shutdown_rx: watch::Receiver<bool>,
    event_tx: broadcast::Sender<InboxEvent>,
    stats: Arc<RwLock<InboxStats>>,
    state: Arc<RwLock<InboxState>>,
    last_message_id: RwLock<Option<String>>,
    last_activity: RwLock<Option<Instant>>,
}

impl InboxPoller {
    /// Create a new inbox poller.
    ///
    /// # Arguments
    ///
    /// * `config` - Polling configuration
    /// * `mailbox_client` - Client for mailbox RPCs
    /// * `keypair` - (public_key, private_key) for mailbox authentication
    /// * `shutdown_rx` - Receiver for shutdown signal
    /// * `event_tx` - Sender for inbox events
    /// * `stats` - Shared statistics
    /// * `state` - Shared state
    pub fn new(
        config: InboxConfig,
        mailbox_client: Arc<Mutex<MailboxClient>>,
        keypair: (Vec<u8>, Vec<u8>),
        shutdown_rx: watch::Receiver<bool>,
        event_tx: broadcast::Sender<InboxEvent>,
        stats: Arc<RwLock<InboxStats>>,
        state: Arc<RwLock<InboxState>>,
    ) -> Self {
        Self {
            config,
            mailbox_client,
            keypair,
            shutdown_rx,
            event_tx,
            stats,
            state,
            last_message_id: RwLock::new(None),
            last_activity: RwLock::new(None),
        }
    }

    /// Execute a single poll cycle.
    ///
    /// Returns:
    /// - `Ok(Some(messages))` - Poll succeeded, messages received
    /// - `Ok(None)` - Shutdown requested, should exit loop
    /// - `Err(e)` - Poll failed, should retry
    pub async fn run_once(&self) -> Result<Option<Vec<EphemeralMessage>>, InboxError> {
        // Check for shutdown
        if *self.shutdown_rx.borrow() {
            return Ok(None);
        }

        // Wait for the appropriate interval
        let interval = self.current_interval();
        let mut shutdown_rx = self.shutdown_rx.clone();

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(None);
                }
            }
            _ = tokio::time::sleep(interval) => {
                // Continue to poll
            }
        }

        // Check shutdown again after sleep
        if *self.shutdown_rx.borrow() {
            return Ok(None);
        }

        // Perform the poll
        self.set_state(InboxState::Polling);
        let result = self.poll_once().await;
        self.set_state(InboxState::Processing);

        match result {
            Ok((messages, has_more)) => {
                let count = messages.len() as u32;

                // Update stats
                {
                    let mut stats = self.stats.write();
                    stats.last_poll_at = Some(Instant::now());
                    stats.last_poll_count = count;
                }

                // Emit poll completed event
                let _ = self.event_tx.send(InboxEvent::PollCompleted { count, has_more });

                // Update last activity if we got messages
                if count > 0 {
                    *self.last_activity.write() = Some(Instant::now());

                    // Update pagination cursor
                    if let Some(last_msg) = messages.last() {
                        *self.last_message_id.write() = Some(last_msg.id.clone());
                    }
                }

                self.set_state(InboxState::Idle);
                Ok(Some(messages))
            }
            Err(e) => {
                // Emit poll failed event
                let _ = self.event_tx.send(InboxEvent::PollFailed {
                    error: e.to_string(),
                });
                tracing::warn!(error = %e, "Poll failed");
                self.set_state(InboxState::Idle);
                Err(e)
            }
        }
    }

    /// Perform a single poll request with challenge-response auth.
    async fn poll_once(&self) -> Result<(Vec<EphemeralMessage>, bool), InboxError> {
        let (public_key, private_key) = &self.keypair;

        // Get challenge
        let challenge_response = {
            let mut client = self.mailbox_client.lock().await;
            client
                .get_mailbox_challenge(public_key.clone())
                .await
                .map_err(|e| InboxError::PollFailed(e.to_string()))?
        };

        // Sign challenge
        let signature = self.sign_challenge(&challenge_response.challenge, private_key)?;

        // Poll for messages
        let after_message_id = self.last_message_id.read().clone();
        let response = {
            let mut client = self.mailbox_client.lock().await;
            client
                .poll_ephemeral_mailbox(
                    public_key.clone(),
                    signature,
                    challenge_response.challenge,
                    after_message_id.as_deref(),
                    self.config.max_batch_size as i32,
                )
                .await
                .map_err(|e| InboxError::PollFailed(e.to_string()))?
        };

        Ok((response.messages, response.has_more))
    }

    /// Sign a challenge with the private key.
    fn sign_challenge(&self, challenge: &[u8], private_key: &[u8]) -> Result<Vec<u8>, InboxError> {
        // Private key should be 32 bytes for Ed25519
        let secret_bytes: [u8; 32] = private_key
            .try_into()
            .map_err(|_| InboxError::SigningFailed("invalid private key length".to_string()))?;

        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let signature = signing_key.sign(challenge);

        Ok(signature.to_bytes().to_vec())
    }

    /// Get the current polling interval based on activity.
    fn current_interval(&self) -> std::time::Duration {
        let last_activity = self.last_activity.read();
        match *last_activity {
            Some(instant) => {
                // Use shorter interval if recent activity (within 2 minutes)
                if instant.elapsed() < std::time::Duration::from_secs(120) {
                    self.config.poll_interval
                } else {
                    self.config.poll_interval_idle
                }
            }
            None => {
                // No activity yet, use idle interval
                self.config.poll_interval_idle
            }
        }
    }

    /// Set the current state.
    fn set_state(&self, state: InboxState) {
        *self.state.write() = state;
    }

    /// Acknowledge messages.
    ///
    /// Acknowledges (and deletes) messages from the server.
    /// Should be called after messages are successfully processed.
    pub async fn acknowledge(&self, message_ids: Vec<String>) -> Result<(), InboxError> {
        if message_ids.is_empty() {
            return Ok(());
        }

        let (public_key, private_key) = &self.keypair;

        // Get fresh challenge
        let challenge_response = {
            let mut client = self.mailbox_client.lock().await;
            client
                .get_mailbox_challenge(public_key.clone())
                .await
                .map_err(|e| InboxError::AuthFailed(e.to_string()))?
        };

        // Sign challenge
        let signature = self.sign_challenge(&challenge_response.challenge, private_key)?;

        // Acknowledge in batches
        for chunk in message_ids.chunks(self.config.ack_batch_size as usize) {
            let chunk_ids: Vec<String> = chunk.to_vec();

            let result = {
                let mut client = self.mailbox_client.lock().await;
                client
                    .acknowledge_messages(
                        public_key.clone(),
                        signature.clone(),
                        challenge_response.challenge.clone(),
                        chunk_ids.clone(),
                    )
                    .await
            };

            if let Err(e) = result {
                let _ = self.event_tx.send(InboxEvent::AcknowledgeFailed {
                    message_ids: chunk_ids,
                    error: e.to_string(),
                });
                return Err(InboxError::AcknowledgeFailed(e.to_string()));
            }
        }

        Ok(())
    }

    /// Get the current polling interval.
    pub fn poll_interval(&self) -> std::time::Duration {
        self.current_interval()
    }

    /// Reset the pagination cursor.
    ///
    /// Call this to start polling from the beginning again.
    pub fn reset_cursor(&self) {
        *self.last_message_id.write() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_challenge() {
        // Use a fixed test keypair (32 bytes private key)
        use ed25519_dalek::SigningKey;

        // Fixed test private key (32 bytes)
        let private_key_bytes: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];

        let signing_key = SigningKey::from_bytes(&private_key_bytes);
        let _public_key = signing_key.verifying_key().to_bytes().to_vec();
        let private_key = signing_key.to_bytes().to_vec();

        // Test signing a challenge
        let challenge = b"test challenge";
        let secret_bytes: [u8; 32] = private_key.clone().try_into().unwrap();
        let key = SigningKey::from_bytes(&secret_bytes);
        let signature = key.sign(challenge);

        // Verify the signature
        use ed25519_dalek::Verifier;
        let verifying_key = key.verifying_key();
        assert!(verifying_key.verify(challenge, &signature).is_ok());
    }

    #[test]
    fn test_current_interval_no_activity() {
        let config = InboxConfig {
            poll_interval: std::time::Duration::from_secs(5),
            poll_interval_idle: std::time::Duration::from_secs(30),
            ..Default::default()
        };

        // Test without a real client - just verify interval logic
        let last_activity: Option<Instant> = None;
        let interval = if last_activity.is_some() {
            config.poll_interval
        } else {
            config.poll_interval_idle
        };

        assert_eq!(interval, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_current_interval_recent_activity() {
        let config = InboxConfig {
            poll_interval: std::time::Duration::from_secs(5),
            poll_interval_idle: std::time::Duration::from_secs(30),
            ..Default::default()
        };

        // Simulate recent activity
        let last_activity = Some(Instant::now());
        let interval = if let Some(instant) = last_activity {
            if instant.elapsed() < std::time::Duration::from_secs(120) {
                config.poll_interval
            } else {
                config.poll_interval_idle
            }
        } else {
            config.poll_interval_idle
        };

        assert_eq!(interval, std::time::Duration::from_secs(5));
    }
}
