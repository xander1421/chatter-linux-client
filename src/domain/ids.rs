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
