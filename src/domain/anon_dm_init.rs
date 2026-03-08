//! Anonymous DM initial contact message (Phase 1).
//!
//! This is the NDSS'21 "init || pk_e" message sent via HPKE encryption.
//! The sender's identity IS included in the encrypted payload per NDSS'21 Section II-A:
//! "can only be decrypted by Bob, who then learns from the payload that the
//! message originated with Alice."
//!
//! # Security Note
//!
//! Phase 1 does NOT have Perfect Forward Secrecy. If the recipient's long-term
//! key is ever compromised, this message could be decrypted. However, since
//! NDSS'21 specifies sender identity is revealed after decryption anyway,
//! including it here is correct protocol behavior.
//!
//! # Wire Format (v4)
//!
//! ```text
//! | version (1) | conversation_id (16) | sender_id (16) | mailbox_pk (32) |
//! | identity_key (33) | delivery_token (12) | name_len (1) | sender_display_name (variable, max 64) |
//! | intro_len (2) | intro_message (variable) | avatar_len (2) | sender_avatar (variable, max 25000) |
//! ```
//!
//! Total fixed size: 1 + 16 + 16 + 32 + 33 + 12 + 1 + 2 + 2 = 115 bytes + display_name + intro + avatar

use super::anon_dm::AnonDmId;
use super::UserId;

/// Current version of the init message format.
pub const INIT_MESSAGE_VERSION: u8 = 4;

/// Previous version (for backward compatibility).
const INIT_MESSAGE_VERSION_V3: u8 = 3;

/// Previous version (for backward compatibility).
const INIT_MESSAGE_VERSION_V2: u8 = 2;

/// Minimum size of init message v4 (without display_name, intro, or avatar).
pub const MIN_INIT_SIZE: usize = 1 + 16 + 16 + 32 + 33 + 12 + 1 + 2 + 2; // 115 bytes

/// Minimum size of init message v3 (legacy).
const MIN_INIT_SIZE_V3: usize = 1 + 16 + 16 + 32 + 33 + 12 + 1 + 2; // 113 bytes

/// Minimum size of init message v2 (legacy).
const MIN_INIT_SIZE_V2: usize = 1 + 16 + 16 + 32 + 33 + 12 + 2; // 112 bytes

/// Maximum intro message length.
pub const MAX_INTRO_LENGTH: usize = 500;

/// Maximum display name length.
pub const MAX_DISPLAY_NAME_LENGTH: usize = 64;

/// Maximum avatar size (2MB to match profile_crypto limit).
pub const MAX_AVATAR_SIZE: usize = 2 * 1024 * 1024;

/// Anonymous DM initial contact message.
///
/// Sent via HPKE encryption to recipient's device inbox.
/// Sender identity is revealed upon decryption per NDSS'21 spec.
#[derive(Clone, Debug)]
pub struct AnonDmInitMessage {
    /// Conversation ID (random UUID).
    pub conversation_id: AnonDmId,
    /// Initiator's user ID (revealed after decryption per NDSS'21).
    pub sender_id: UserId,
    /// Initiator's ephemeral mailbox public key (32 bytes Ed25519).
    /// Recipient sends acceptance to this mailbox.
    pub mailbox_pk: [u8; 32],
    /// Initiator's Signal Protocol identity key (33 bytes).
    /// Used by recipient to establish Double Ratchet session in Phase 2.
    pub identity_key: [u8; 33],
    /// Initiator's delivery token (12 bytes / 96 bits).
    /// Required for recipient to send sealed sender messages.
    pub delivery_token: [u8; 12],
    /// Initiator's display name (revealed after decryption).
    /// Max 64 chars. Empty string if unknown.
    pub sender_display_name: String,
    /// Optional intro message (max 500 chars).
    pub intro_message: String,
    /// Optional sender avatar (max 25KB).
    pub sender_avatar: Option<Vec<u8>>,
}

/// Error parsing an init message.
#[derive(Debug, thiserror::Error)]
pub enum InitMessageError {
    /// Invalid data.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Unsupported version.
    #[error("unsupported version: {0}")]
    UnsupportedVersion(u8),

    /// Message too short.
    #[error("message too short: {0} bytes, need at least {1}")]
    TooShort(usize, usize),

    /// Intro message too long.
    #[error("intro message too long: {0} bytes, max {1}")]
    IntroTooLong(usize, usize),

    /// Display name too long.
    #[error("display name too long: {0} bytes, max {1}")]
    DisplayNameTooLong(usize, usize),

    /// Avatar too large.
    #[error("avatar too large: {0} bytes, max {1}")]
    AvatarTooLarge(usize, usize),
}

/// Error for avatar too large.
#[derive(Debug, thiserror::Error)]
#[error("avatar too large: {0} bytes, max {1}")]
pub struct AvatarTooLargeError(pub usize, pub usize);

impl AnonDmInitMessage {
    /// Create a new init message.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conversation_id: AnonDmId,
        sender_id: UserId,
        mailbox_pk: [u8; 32],
        identity_key: [u8; 33],
        delivery_token: [u8; 12],
        sender_display_name: impl Into<String>,
        intro_message: impl Into<String>,
        sender_avatar: Option<Vec<u8>>,
    ) -> Self {
        Self {
            conversation_id,
            sender_id,
            mailbox_pk,
            identity_key,
            delivery_token,
            sender_display_name: sender_display_name.into(),
            intro_message: intro_message.into(),
            sender_avatar,
        }
    }

    /// Serialize to bytes (v4 format).
    pub fn to_bytes(&self) -> Result<Vec<u8>, InitMessageError> {
        let name_bytes = self.sender_display_name.as_bytes();
        if name_bytes.len() > MAX_DISPLAY_NAME_LENGTH {
            return Err(InitMessageError::DisplayNameTooLong(name_bytes.len(), MAX_DISPLAY_NAME_LENGTH));
        }

        let intro_bytes = self.intro_message.as_bytes();
        if intro_bytes.len() > MAX_INTRO_LENGTH {
            return Err(InitMessageError::IntroTooLong(intro_bytes.len(), MAX_INTRO_LENGTH));
        }

        let avatar_bytes = self.sender_avatar.as_deref().unwrap_or(&[]);
        if avatar_bytes.len() > MAX_AVATAR_SIZE {
            return Err(InitMessageError::AvatarTooLarge(avatar_bytes.len(), MAX_AVATAR_SIZE));
        }

        let total_size = MIN_INIT_SIZE + name_bytes.len() + intro_bytes.len() + avatar_bytes.len();
        let mut bytes = Vec::with_capacity(total_size);

        // Version
        bytes.push(INIT_MESSAGE_VERSION);

        // Conversation ID (16 bytes)
        bytes.extend_from_slice(self.conversation_id.as_bytes());

        // Sender ID (16 bytes) - revealed after decryption per NDSS'21
        bytes.extend_from_slice(self.sender_id.as_bytes());

        // Mailbox public key (32 bytes)
        bytes.extend_from_slice(&self.mailbox_pk);

        // Identity key (33 bytes)
        bytes.extend_from_slice(&self.identity_key);

        // Delivery token (12 bytes)
        bytes.extend_from_slice(&self.delivery_token);

        // Sender display name length (1 byte)
        bytes.push(name_bytes.len() as u8);

        // Sender display name
        bytes.extend_from_slice(name_bytes);

        // Intro message length (2 bytes, big-endian)
        let intro_len = intro_bytes.len() as u16;
        bytes.extend_from_slice(&intro_len.to_be_bytes());

        // Intro message
        bytes.extend_from_slice(intro_bytes);

        // Avatar length (2 bytes, big-endian)
        let avatar_len = avatar_bytes.len() as u16;
        bytes.extend_from_slice(&avatar_len.to_be_bytes());

        // Avatar bytes
        bytes.extend_from_slice(avatar_bytes);

        Ok(bytes)
    }

    /// Parse from bytes (supports v2, v3 and v4 formats).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, InitMessageError> {
        if bytes.is_empty() {
            return Err(InitMessageError::TooShort(0, MIN_INIT_SIZE_V2));
        }

        let version = bytes[0];

        // Route to appropriate parser based on version
        match version {
            INIT_MESSAGE_VERSION => Self::from_bytes_v4(bytes),
            INIT_MESSAGE_VERSION_V3 => Self::from_bytes_v3(bytes),
            INIT_MESSAGE_VERSION_V2 => Self::from_bytes_v2(bytes),
            _ => Err(InitMessageError::UnsupportedVersion(version)),
        }
    }

    /// Parse v4 format (with display_name and avatar).
    fn from_bytes_v4(bytes: &[u8]) -> Result<Self, InitMessageError> {
        if bytes.len() < MIN_INIT_SIZE {
            return Err(InitMessageError::TooShort(bytes.len(), MIN_INIT_SIZE));
        }

        let mut offset = 1; // Skip version byte

        // Conversation ID (16 bytes)
        let mut conv_id_bytes = [0u8; 16];
        conv_id_bytes.copy_from_slice(&bytes[offset..offset + 16]);
        let conversation_id = AnonDmId::from_bytes(conv_id_bytes);
        offset += 16;

        // Sender ID (16 bytes)
        let sender_id = UserId::try_from_slice(&bytes[offset..offset + 16])
            .ok_or_else(|| InitMessageError::InvalidData("invalid sender_id".into()))?;
        offset += 16;

        // Mailbox public key (32 bytes)
        let mut mailbox_pk = [0u8; 32];
        mailbox_pk.copy_from_slice(&bytes[offset..offset + 32]);
        offset += 32;

        // Identity key (33 bytes)
        let mut identity_key = [0u8; 33];
        identity_key.copy_from_slice(&bytes[offset..offset + 33]);
        offset += 33;

        // Validate identity key format
        if identity_key[0] != 0x05 {
            return Err(InitMessageError::InvalidData(format!(
                "invalid identity key type byte: 0x{:02x}",
                identity_key[0]
            )));
        }

        // Delivery token (12 bytes)
        let mut delivery_token = [0u8; 12];
        delivery_token.copy_from_slice(&bytes[offset..offset + 12]);
        offset += 12;

        // Display name length (1 byte)
        let name_len = bytes[offset] as usize;
        offset += 1;

        if name_len > MAX_DISPLAY_NAME_LENGTH {
            return Err(InitMessageError::DisplayNameTooLong(name_len, MAX_DISPLAY_NAME_LENGTH));
        }
        if bytes.len() < offset + name_len {
            return Err(InitMessageError::TooShort(bytes.len(), offset + name_len));
        }

        // Display name
        let sender_display_name = String::from_utf8(bytes[offset..offset + name_len].to_vec())
            .map_err(|_| InitMessageError::InvalidData("invalid UTF-8 in display name".into()))?;
        offset += name_len;

        // Intro message length (2 bytes)
        if bytes.len() < offset + 2 {
            return Err(InitMessageError::TooShort(bytes.len(), offset + 2));
        }
        let intro_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += 2;

        // Intro message
        if intro_len > MAX_INTRO_LENGTH {
            return Err(InitMessageError::IntroTooLong(intro_len, MAX_INTRO_LENGTH));
        }
        if bytes.len() < offset + intro_len {
            return Err(InitMessageError::TooShort(bytes.len(), offset + intro_len));
        }
        let intro_message = String::from_utf8(bytes[offset..offset + intro_len].to_vec())
            .map_err(|_| InitMessageError::InvalidData("invalid UTF-8 in intro message".into()))?;
        offset += intro_len;

        // Avatar length (2 bytes)
        if bytes.len() < offset + 2 {
            return Err(InitMessageError::TooShort(bytes.len(), offset + 2));
        }
        let avatar_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += 2;

        // Avatar
        let sender_avatar = if avatar_len > 0 {
            if avatar_len > MAX_AVATAR_SIZE {
                return Err(InitMessageError::AvatarTooLarge(avatar_len, MAX_AVATAR_SIZE));
            }
            if bytes.len() < offset + avatar_len {
                return Err(InitMessageError::TooShort(bytes.len(), offset + avatar_len));
            }
            Some(bytes[offset..offset + avatar_len].to_vec())
        } else {
            None
        };

        Ok(Self {
            conversation_id,
            sender_id,
            mailbox_pk,
            identity_key,
            delivery_token,
            sender_display_name,
            intro_message,
            sender_avatar,
        })
    }

    /// Parse v3 format (with display_name, no avatar).
    fn from_bytes_v3(bytes: &[u8]) -> Result<Self, InitMessageError> {
        if bytes.len() < MIN_INIT_SIZE_V3 {
            return Err(InitMessageError::TooShort(bytes.len(), MIN_INIT_SIZE_V3));
        }

        let mut offset = 1; // Skip version byte

        // Conversation ID (16 bytes)
        let mut conv_id_bytes = [0u8; 16];
        conv_id_bytes.copy_from_slice(&bytes[offset..offset + 16]);
        let conversation_id = AnonDmId::from_bytes(conv_id_bytes);
        offset += 16;

        // Sender ID (16 bytes)
        let sender_id = UserId::try_from_slice(&bytes[offset..offset + 16])
            .ok_or_else(|| InitMessageError::InvalidData("invalid sender_id".into()))?;
        offset += 16;

        // Mailbox public key (32 bytes)
        let mut mailbox_pk = [0u8; 32];
        mailbox_pk.copy_from_slice(&bytes[offset..offset + 32]);
        offset += 32;

        // Identity key (33 bytes)
        let mut identity_key = [0u8; 33];
        identity_key.copy_from_slice(&bytes[offset..offset + 33]);
        offset += 33;

        // Validate identity key format
        if identity_key[0] != 0x05 {
            return Err(InitMessageError::InvalidData(format!(
                "invalid identity key type byte: 0x{:02x}",
                identity_key[0]
            )));
        }

        // Delivery token (12 bytes)
        let mut delivery_token = [0u8; 12];
        delivery_token.copy_from_slice(&bytes[offset..offset + 12]);
        offset += 12;

        // Display name length (1 byte)
        let name_len = bytes[offset] as usize;
        offset += 1;

        if name_len > MAX_DISPLAY_NAME_LENGTH {
            return Err(InitMessageError::DisplayNameTooLong(name_len, MAX_DISPLAY_NAME_LENGTH));
        }
        if bytes.len() < offset + name_len {
            return Err(InitMessageError::TooShort(bytes.len(), offset + name_len));
        }

        // Display name
        let sender_display_name = String::from_utf8(bytes[offset..offset + name_len].to_vec())
            .map_err(|_| InitMessageError::InvalidData("invalid UTF-8 in display name".into()))?;
        offset += name_len;

        // Intro message length (2 bytes)
        if bytes.len() < offset + 2 {
            return Err(InitMessageError::TooShort(bytes.len(), offset + 2));
        }
        let intro_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += 2;

        // Intro message
        if intro_len > MAX_INTRO_LENGTH {
            return Err(InitMessageError::IntroTooLong(intro_len, MAX_INTRO_LENGTH));
        }
        if bytes.len() < offset + intro_len {
            return Err(InitMessageError::TooShort(bytes.len(), offset + intro_len));
        }
        let intro_message = String::from_utf8(bytes[offset..offset + intro_len].to_vec())
            .map_err(|_| InitMessageError::InvalidData("invalid UTF-8 in intro message".into()))?;

        Ok(Self {
            conversation_id,
            sender_id,
            mailbox_pk,
            identity_key,
            delivery_token,
            sender_display_name,
            intro_message,
            sender_avatar: None, // V3 format doesn't include avatar
        })
    }

    /// Parse v2 format (legacy, without display_name).
    fn from_bytes_v2(bytes: &[u8]) -> Result<Self, InitMessageError> {
        if bytes.len() < MIN_INIT_SIZE_V2 {
            return Err(InitMessageError::TooShort(bytes.len(), MIN_INIT_SIZE_V2));
        }

        let mut offset = 1; // Skip version byte

        // Conversation ID (16 bytes)
        let mut conv_id_bytes = [0u8; 16];
        conv_id_bytes.copy_from_slice(&bytes[offset..offset + 16]);
        let conversation_id = AnonDmId::from_bytes(conv_id_bytes);
        offset += 16;

        // Sender ID (16 bytes)
        let sender_id = UserId::try_from_slice(&bytes[offset..offset + 16])
            .ok_or_else(|| InitMessageError::InvalidData("invalid sender_id".into()))?;
        offset += 16;

        // Mailbox public key (32 bytes)
        let mut mailbox_pk = [0u8; 32];
        mailbox_pk.copy_from_slice(&bytes[offset..offset + 32]);
        offset += 32;

        // Identity key (33 bytes)
        let mut identity_key = [0u8; 33];
        identity_key.copy_from_slice(&bytes[offset..offset + 33]);
        offset += 33;

        // Validate identity key format
        if identity_key[0] != 0x05 {
            return Err(InitMessageError::InvalidData(format!(
                "invalid identity key type byte: 0x{:02x}",
                identity_key[0]
            )));
        }

        // Delivery token (12 bytes)
        let mut delivery_token = [0u8; 12];
        delivery_token.copy_from_slice(&bytes[offset..offset + 12]);
        offset += 12;

        // Intro message length (2 bytes)
        if bytes.len() < offset + 2 {
            return Err(InitMessageError::TooShort(bytes.len(), offset + 2));
        }
        let intro_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += 2;

        // Intro message
        if intro_len > MAX_INTRO_LENGTH {
            return Err(InitMessageError::IntroTooLong(intro_len, MAX_INTRO_LENGTH));
        }
        if bytes.len() < offset + intro_len {
            return Err(InitMessageError::TooShort(bytes.len(), offset + intro_len));
        }
        let intro_message = String::from_utf8(bytes[offset..offset + intro_len].to_vec())
            .map_err(|_| InitMessageError::InvalidData("invalid UTF-8 in intro message".into()))?;

        Ok(Self {
            conversation_id,
            sender_id,
            mailbox_pk,
            identity_key,
            delivery_token,
            sender_display_name: String::new(), // Empty for v2 messages
            intro_message,
            sender_avatar: None, // V2 format doesn't include avatar
        })
    }

    /// Get the X25519 public key from the identity key.
    pub fn x25519_identity_key(&self) -> [u8; 32] {
        let mut key = [0u8; 32];
        key.copy_from_slice(&self.identity_key[1..33]);
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let mut mailbox_pk = [0u8; 32];
        let mut identity_key = [0u8; 33];
        let mut delivery_token = [0u8; 12];

        rand::RngCore::fill_bytes(&mut rand::rng(), &mut mailbox_pk);
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut identity_key[1..]);
        identity_key[0] = 0x05;
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut delivery_token);

        let msg = AnonDmInitMessage::new(
            AnonDmId::new(),
            UserId::new(),
            mailbox_pk,
            identity_key,
            delivery_token,
            "TestUser",
            "Hello, let's chat!",
        );

        let bytes = msg.to_bytes().unwrap();
        let parsed = AnonDmInitMessage::from_bytes(&bytes).unwrap();

        assert_eq!(msg.conversation_id, parsed.conversation_id);
        assert_eq!(msg.sender_id, parsed.sender_id);
        assert_eq!(msg.mailbox_pk, parsed.mailbox_pk);
        assert_eq!(msg.identity_key, parsed.identity_key);
        assert_eq!(msg.delivery_token, parsed.delivery_token);
        assert_eq!(msg.sender_display_name, parsed.sender_display_name);
        assert_eq!(msg.intro_message, parsed.intro_message);
    }

    #[test]
    fn test_empty_intro() {
        let mailbox_pk = [0u8; 32];
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x05;
        let delivery_token = [0u8; 12];

        let msg = AnonDmInitMessage::new(
            AnonDmId::new(),
            UserId::new(),
            mailbox_pk,
            identity_key,
            delivery_token,
            "",
            "",
        );

        let bytes = msg.to_bytes().unwrap();
        // MIN_INIT_SIZE now includes display_name length byte
        assert!(bytes.len() >= MIN_INIT_SIZE);

        let parsed = AnonDmInitMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.sender_display_name, "");
        assert_eq!(parsed.intro_message, "");
    }

    #[test]
    fn test_max_intro() {
        let mailbox_pk = [0u8; 32];
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x05;
        let delivery_token = [0u8; 12];

        let intro = "x".repeat(MAX_INTRO_LENGTH);
        let msg = AnonDmInitMessage::new(
            AnonDmId::new(),
            UserId::new(),
            mailbox_pk,
            identity_key,
            delivery_token,
            "MaxIntroUser",
            &intro,
        );

        let bytes = msg.to_bytes().unwrap();
        let parsed = AnonDmInitMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.sender_display_name, "MaxIntroUser");
        assert_eq!(parsed.intro_message, intro);
    }

    #[test]
    fn test_intro_too_long() {
        let mailbox_pk = [0u8; 32];
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x05;
        let delivery_token = [0u8; 12];

        let intro = "x".repeat(MAX_INTRO_LENGTH + 1);
        let msg = AnonDmInitMessage::new(
            AnonDmId::new(),
            UserId::new(),
            mailbox_pk,
            identity_key,
            delivery_token,
            "TooLongUser",
            &intro,
        );

        assert!(msg.to_bytes().is_err());
    }

    #[test]
    fn test_invalid_version() {
        let mut bytes = vec![4u8]; // Version 4 (unsupported)
        bytes.extend_from_slice(&[0u8; MIN_INIT_SIZE - 1]);

        assert!(AnonDmInitMessage::from_bytes(&bytes).is_err());
    }
}
