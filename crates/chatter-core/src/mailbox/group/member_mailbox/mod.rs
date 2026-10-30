//! Per-member group mailbox state and management.
//!
//! Per-member, per-group ephemeral mailbox management for privacy-preserving group messaging.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use uuid::Uuid;

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::storage::Database;

/// Our ephemeral mailbox for a specific group.
///
/// Created using a blind-signed token from TokenManager.
/// Each group we're in has a unique mailbox address to prevent
/// server correlation of group membership.
#[derive(Clone)]
pub struct GroupMailbox {
    /// Unique mailbox identifier.
    pub id: String,
    /// Group this mailbox is for.
    pub group_id: ChannelId,
    /// Ed25519 public key (32 bytes) - mailbox address.
    pub public_key: [u8; 32],
    /// Ed25519 private key (64 bytes) - for signing challenges.
    pub private_key: [u8; 64],
    /// Unblinded RSA signature over the public key.
    pub signature: Vec<u8>,
    /// Token ID this mailbox was created from (for tracking).
    pub token_id: String,
    /// When this mailbox was created.
    pub created_at: DateTime<Utc>,
    /// When server will delete this mailbox.
    pub expires_at: DateTime<Utc>,
    /// Last time we polled this mailbox for messages.
    pub last_polled_at: Option<DateTime<Utc>>,
}

/// Another member's mailbox address in a group.
///
/// Received when members exchange mailbox addresses within the group.
#[derive(Clone)]
pub struct MemberMailbox {
    /// The member's user ID.
    pub user_id: UserId,
    /// Their Ed25519 public key (32 bytes) - their mailbox address.
    pub public_key: [u8; 32],
    /// When we received this address.
    pub received_at: DateTime<Utc>,
    /// When they said it expires (if known).
    pub expires_at: Option<DateTime<Utc>>,
}

/// Full mailbox state for a group.
///
/// Tracks our mailbox (for receiving) and all member mailboxes (for sending).
pub struct GroupMailboxState {
    /// Group this state is for.
    pub group_id: ChannelId,
    /// Our mailbox for receiving messages in this group.
    pub our_mailbox: Option<GroupMailbox>,
    /// Member mailboxes for sending to other group members.
    pub member_mailboxes: HashMap<UserId, MemberMailbox>,
}

impl GroupMailbox {
    /// Check if this mailbox has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    /// Check if this mailbox expires soon (within given duration).
    pub fn expires_soon(&self, within: Duration) -> bool {
        Utc::now() + within >= self.expires_at
    }

    /// Sign a challenge with our Ed25519 private key.
    ///
    /// Used for challenge-response authentication when polling.
    pub fn sign_challenge(&self, challenge: &[u8]) -> Result<Vec<u8>, MailboxError> {
        // Reconstruct signing key from stored bytes
        // Ed25519 "private key" is 64 bytes: 32-byte secret + 32-byte public
        if self.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(format!(
                "Invalid private key length: expected 64, got {}",
                self.private_key.len()
            )));
        }

        // Extract the 32-byte secret key portion
        let secret_bytes: [u8; 32] = self.private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::InvalidServerKey("Failed to extract secret key".into()))?;

        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let signature: Signature = signing_key.sign(challenge);

        Ok(signature.to_bytes().to_vec())
    }

    /// Get the verifying (public) key for this mailbox.
    pub fn verifying_key(&self) -> Result<VerifyingKey, MailboxError> {
        VerifyingKey::from_bytes(&self.public_key)
            .map_err(|e| MailboxError::InvalidServerKey(format!("Invalid public key: {}", e)))
    }
}

impl MemberMailbox {
    /// Create a new member mailbox from received public key.
    pub fn new(user_id: UserId, public_key: [u8; 32], expires_at: Option<DateTime<Utc>>) -> Self {
        Self {
            user_id,
            public_key,
            received_at: Utc::now(),
            expires_at,
        }
    }

    /// Check if this member mailbox might be expired.
    pub fn is_possibly_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Utc::now() >= exp)
    }
}

impl GroupMailboxState {
    /// Create a new group mailbox state for a group.
    pub fn new(group_id: ChannelId) -> Self {
        Self {
            group_id,
            our_mailbox: None,
            member_mailboxes: HashMap::new(),
        }
    }

    /// Check if we need to create a mailbox for this group.
    pub fn needs_mailbox(&self) -> bool {
        self.our_mailbox.is_none()
    }

    /// Check if we're ready to send messages (have member mailboxes).
    pub fn is_ready_to_send(&self) -> bool {
        !self.member_mailboxes.is_empty()
    }

    /// Check if we can receive messages (have non-expired mailbox).
    pub fn is_ready_to_receive(&self) -> bool {
        self.our_mailbox.as_ref().is_some_and(|m| !m.is_expired())
    }

    /// Get a specific member's mailbox.
    pub fn get_member_mailbox(&self, user_id: &UserId) -> Option<&MemberMailbox> {
        self.member_mailboxes.get(user_id)
    }

    /// Get count of member mailboxes.
    pub fn member_count(&self) -> usize {
        self.member_mailboxes.len()
    }

    /// Check if our mailbox needs refresh (expiring soon).
    ///
    /// Returns true if mailbox expires within 24 hours.
    pub fn mailbox_needs_refresh(&self) -> bool {
        self.our_mailbox
            .as_ref()
            .is_some_and(|m| m.expires_soon(Duration::hours(24)))
    }
}

/// Manager for group mailbox state persistence.
///
/// Handles database operations for storing and retrieving group mailbox state.
pub struct GroupMailboxManager {
    /// Database connection for state storage.
    db: Arc<Database>,
}

mod lookup;
mod pending;
mod persistence;

#[cfg(test)]
mod tests;
