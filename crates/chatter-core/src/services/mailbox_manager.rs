//! Ephemeral mailbox delivery for NDSS '21 privacy-preserving messaging.
//!
//! Sends sealed envelopes to a peer's ephemeral mailbox over the anonymous
//! (no auth headers) mailbox client.

use std::sync::Arc;

use crate::error::NetworkError;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::mailbox_v1::EnvelopeType;

/// Errors that can occur during mailbox operations.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),
}

/// Sends messages to ephemeral mailboxes.
///
/// Implements the send half of the NDSS '21 mailbox system for sender anonymity.
pub struct MailboxManager {
    /// gRPC client pool (for creating anonymous MailboxClient instances)
    client_pool: Arc<GrpcClientPool>,
}

impl MailboxManager {
    /// Create a new MailboxManager.
    pub fn new(client_pool: Arc<GrpcClientPool>) -> Self {
        Self { client_pool }
    }

    /// Send a message to an ephemeral mailbox.
    ///
    /// Uses the anonymous mailbox client so the send does NOT reveal user identity.
    ///
    /// # Arguments
    ///
    /// * `destination_public_key` - Public key of the destination mailbox
    /// * `encrypted_envelope` - Encrypted message envelope (sealed sender format)
    /// * `envelope_type` - Type of envelope (SealedSender, PrekeyMessage, etc.)
    pub async fn send_to_mailbox(
        &self,
        destination_public_key: &[u8],
        encrypted_envelope: Vec<u8>,
        envelope_type: EnvelopeType,
    ) -> Result<(), MailboxError> {
        let mut client = self.client_pool.anonymous_mailbox_client();

        let _response = client
            .send_to_ephemeral_mailbox(
                destination_public_key.to_vec(),
                encrypted_envelope,
                envelope_type as i32,
            )
            .await?;

        Ok(())
    }
}
