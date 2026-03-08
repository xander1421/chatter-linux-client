//! Strongly-typed ID wrappers.
//!
//! Type-safe identifiers for domain entities preventing accidental misuse.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Unique identifier for a user (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct UserId(pub Uuid);

/// Unique identifier for a channel (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

/// Unique identifier for a message (UUID v4 or ULID).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct MessageId(pub Uuid);

/// Unique identifier for a device (u32, matches Signal protocol).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct DeviceId(pub u32);

/// Unique identifier for an attachment (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct AttachmentId(pub Uuid);

/// Unique identifier for a call (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct CallId(pub Uuid);

/// Unique identifier for a session (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

/// Unique identifier for a reaction (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ReactionId(pub Uuid);

/// Unique identifier for an AI integration (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct IntegrationId(pub Uuid);

/// Unique identifier for a notification (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct NotificationId(pub Uuid);

/// Unique identifier for a draft (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct DraftId(pub Uuid);

/// Unique identifier for a saved message in personal vault (UUID v4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct SavedMessageId(pub Uuid);

/// Unique identifier for a thread (same as MessageId of root message).
pub type ThreadId = MessageId;

// ============================================================================
// UUID-based ID implementations
// ============================================================================

macro_rules! impl_uuid_id {
    ($($ty:ident),+) => {
        $(
            impl $ty {
                /// Generate a new random ID.
                ///
                /// Note: We intentionally do not implement Default for IDs.
                /// IDs should always be explicitly created to avoid accidental
                /// use of uninitialized/default identifiers.
                #[must_use]
                #[allow(clippy::new_without_default)]
                pub fn new() -> Self {
                    Self(Uuid::new_v4())
                }

                /// Wrap an existing UUID.
                #[must_use]
                pub fn from_uuid(uuid: Uuid) -> Self {
                    Self(uuid)
                }

                /// Get a reference to the inner UUID.
                #[must_use]
                pub fn as_uuid(&self) -> &Uuid {
                    &self.0
                }

                /// Consume self and return the inner UUID.
                #[must_use]
                pub fn into_uuid(self) -> Uuid {
                    self.0
                }

                /// Get as raw bytes (16 bytes).
                #[must_use]
                pub fn as_bytes(&self) -> &[u8; 16] {
                    self.0.as_bytes()
                }

                /// Try to create from a byte slice.
                #[must_use]
                pub fn try_from_slice(bytes: &[u8]) -> Option<Self> {
                    if bytes.len() != 16 {
                        return None;
                    }
                    let mut arr = [0u8; 16];
                    arr.copy_from_slice(bytes);
                    Some(Self(Uuid::from_bytes(arr)))
                }
            }

            impl fmt::Display for $ty {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "{}", self.0)
                }
            }

            impl FromStr for $ty {
                type Err = uuid::Error;

                fn from_str(s: &str) -> Result<Self, Self::Err> {
                    Uuid::parse_str(s).map(Self)
                }
            }

            impl From<Uuid> for $ty {
                fn from(uuid: Uuid) -> Self {
                    Self(uuid)
                }
            }

            impl From<$ty> for Uuid {
                fn from(id: $ty) -> Self {
                    id.0
                }
            }
        )+
    };
}

impl_uuid_id!(
    UserId,
    ChannelId,
    MessageId,
    AttachmentId,
    CallId,
    SessionId,
    ReactionId,
    IntegrationId,
    NotificationId,
    DraftId,
    SavedMessageId
);

// ============================================================================
// ChannelId helpers
// ============================================================================

impl ChannelId {
    /// Create a deterministic channel ID from two user IDs for DM conversations.
    ///
    /// The channel ID is derived by sorting the two user IDs lexicographically
    /// and hashing them together. This ensures the same channel ID is produced
    /// regardless of which user initiates the conversation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let channel_id = ChannelId::from_dm_participants(&user_a, &user_b);
    /// // Same result regardless of order
    /// assert_eq!(channel_id, ChannelId::from_dm_participants(&user_b, &user_a));
    /// ```
    /// Create a deterministic channel ID for DM conversations.
    ///
    /// **IMPORTANT**: This must match the server's `GenerateDMChannelID` function exactly.
    /// Server uses: `uuid.NewSHA1(uuid.Nil, []byte("dm_channel_{user1}_{user2}"))`
    /// where user1 < user2 lexicographically (string comparison).
    #[must_use]
    pub fn from_dm_participants(user_a: &UserId, user_b: &UserId) -> Self {
        // Sort user IDs lexicographically (as strings) - must match server's Go string comparison
        let user_a_str = user_a.0.to_string();
        let user_b_str = user_b.0.to_string();

        let (first, second) = if user_a_str > user_b_str {
            (&user_b_str, &user_a_str)
        } else {
            (&user_a_str, &user_b_str)
        };

        // Create input string matching server: "dm_channel_{user1}_{user2}"
        let input = format!("dm_channel_{}_{}", first, second);

        // Generate UUID v5 with NULL namespace (matches server's uuid.Nil)
        // UUID v5 uses SHA1 internally, matching server's uuid.NewSHA1
        let dm_uuid = Uuid::new_v5(&Uuid::nil(), input.as_bytes());

        Self(dm_uuid)
    }
}

/// Convert UserId to ChannelId.
///
/// Used for saved messages channels where channel_id = user_id.
impl From<UserId> for ChannelId {
    fn from(user_id: UserId) -> Self {
        Self(user_id.0)
    }
}

// ============================================================================
// MessageId helpers
// ============================================================================

impl MessageId {
    /// Create a deterministic MessageId from a timestamp.
    ///
    /// Used for Signal-style message identification where messages are
    /// identified by (sender, timestamp) pairs. This creates a consistent
    /// UUID from the timestamp for event emission.
    #[must_use]
    pub fn from_timestamp(timestamp: u64) -> Self {
        // Create a deterministic UUID from timestamp using namespace hashing
        let input = format!("msg_timestamp:{}", timestamp);
        let uuid = Uuid::new_v5(&Uuid::nil(), input.as_bytes());
        Self(uuid)
    }
}

// ============================================================================
// AttachmentId helpers
// ============================================================================

impl AttachmentId {
    /// Create a deterministic attachment ID from a CDN key.
    ///
    /// Used for DM attachments where the server assigns a cdn_key rather than
    /// a UUID. This generates a consistent UUID from the cdn_key so we can
    /// look up the attachment in the repository.
    #[must_use]
    pub fn from_cdn_key(cdn_key: &str) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(b"cdn_key:");
        hasher.update(cdn_key.as_bytes());
        let hash = hasher.finalize();

        // Convert first 16 bytes to UUID (version 5 namespace format)
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&hash[..16]);
        // Set version 5 (SHA-1 namespace) and variant 1 (RFC 4122)
        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x50;
        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;

        Self(Uuid::from_bytes(uuid_bytes))
    }
}

// ============================================================================
// DeviceId implementation (u32-based)
// ============================================================================

impl DeviceId {
    /// The primary device ID (Signal protocol convention).
    pub const PRIMARY: Self = Self(1);

    /// Create a new DeviceId from a u32.
    #[must_use]
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    /// Get the inner u32 value.
    #[must_use]
    pub fn as_u32(&self) -> u32 {
        self.0
    }

    /// Check if this is the primary device.
    #[must_use]
    pub fn is_primary(&self) -> bool {
        self.0 == 1
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for DeviceId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u32>().map(Self)
    }
}

impl From<u32> for DeviceId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl From<DeviceId> for u32 {
    fn from(id: DeviceId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_id_roundtrip() {
        let id = UserId::new();
        let s = id.to_string();
        let parsed: UserId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_device_id() {
        let device = DeviceId::new(1);
        assert!(device.is_primary());
        assert_eq!(device.as_u32(), 1);
        assert_eq!(device.to_string(), "1");

        let parsed: DeviceId = "42".parse().unwrap();
        assert_eq!(parsed.as_u32(), 42);
    }
}
