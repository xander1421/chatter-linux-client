//! Shared primitives for anonymous messaging (NDSS '21).
//!
//! Provides generic identifiers and structures used by both Anon DM
//! and Anon Group infrastructures. Uses phantom types to preserve
//! type safety while eliminating boilerplate duplication.

use std::marker::PhantomData;
use std::str::FromStr;
use std::fmt;
use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Generic Identifiers
// ============================================================================

/// Generic UUID-based identifier for anonymous entities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnonId<T>(Uuid, PhantomData<T>);

impl<T> AnonId<T> {
    /// Create a new random ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4(), PhantomData)
    }

    /// Create from a UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid, PhantomData)
    }

    /// Create from raw bytes (16 bytes).
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes), PhantomData)
    }

    /// Try to create from a byte slice.
    pub fn try_from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 16 {
            return None;
        }
        let mut arr = [0u8; 16];
        arr.copy_from_slice(bytes);
        Some(Self::from_bytes(arr))
    }

    /// Get the underlying UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Get as raw bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Get as byte vector.
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }
}

impl<T> Default for AnonId<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Display for AnonId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<T> FromStr for AnonId<T> {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?, PhantomData))
    }
}

impl<T> From<AnonId<T>> for super::ids::ChannelId {
    fn from(id: AnonId<T>) -> Self {
        super::ids::ChannelId::from_uuid(id.0)
    }
}

/// Generic string-based message identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnonMessageId<T>(String, PhantomData<T>);

impl<T> AnonMessageId<T> {
    /// Create a new message ID (random UUID string).
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string(), PhantomData)
    }

    /// Create from a string.
    pub fn from_string(s: String) -> Self {
        Self(s, PhantomData)
    }

    /// Get the ID as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<T> Default for AnonMessageId<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Display for AnonMessageId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Shared Mailbox
// ============================================================================

/// A client-side managed mailbox for anonymous communication.
#[derive(Debug, Clone)]
pub struct AnonMailbox<T> {
    /// Unique row ID in local storage.
    pub id: String,
    /// The conversation or group this mailbox is for.
    pub parent_id: AnonId<T>,
    /// Our public key for this mailbox (32 bytes Ed25519).
    pub public_key: [u8; 32],
    /// Our private key for this mailbox (64 bytes Ed25519 keypair).
    pub private_key: [u8; 64],
    /// The blind signature token ID used to register this mailbox.
    pub token_id: String,
    /// When this mailbox was created.
    pub created_at: DateTime<Utc>,
    /// When this mailbox expires.
    pub expires_at: DateTime<Utc>,
}

impl<T> AnonMailbox<T> {
    /// Check if this mailbox has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

// ============================================================================
// Rate Limiting
// ============================================================================

/// Shared structure for tracking rate limits in anonymous conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnonRateLimitEntry {
    /// Entity type ("dm" or "group").
    pub entity_type: String,
    /// Entity ID (binary).
    pub entity_id: Vec<u8>,
    /// Number of messages sent in current window.
    pub message_count: u32,
    /// When the current window started.
    pub window_start: DateTime<Utc>,
    /// When this entry was last updated.
    pub updated_at: DateTime<Utc>,
}

impl AnonRateLimitEntry {
    /// Create a new rate limit entry.
    pub fn new(entity_type: impl Into<String>, entity_id: Vec<u8>) -> Self {
        let now = Utc::now();
        Self {
            entity_type: entity_type.into(),
            entity_id,
            message_count: 1,
            window_start: now,
            updated_at: now,
        }
    }
}
