//! Private friend code for NDSS'21-compliant anonymous DM.
//!
//! Unlike regular friend codes which only contain a user ID (requiring server
//! lookup for keys), private friend codes embed all cryptographic material
//! needed for initial contact. This follows the NDSS'21 specification where
//! "Alice and Bob have verified their respective keys out of band."
//!
//! # Wire Format
//!
//! ```text
//! | user_id (16) | identity_key (33) | delivery_token (12) |
//! ```
//!
//! - user_id: UUID as 16 bytes
//! - identity_key: Signal Protocol identity key (0x05 prefix + 32-byte Curve25519)
//! - delivery_token: 96-bit token derived from profile key (HKDF)
//!
//! Total: 61 bytes → ~98 characters when base32 encoded.
//!
//! # URL Format
//!
//! `chatter://private/{base32_encoded_data}`
//!
//! # Security Properties
//!
//! - **No Server Lookup**: Keys are embedded, avoiding metadata leakage
//! - **Out-of-Band Verification**: User visually verifies code authenticity
//! - **Delivery Token**: Required for sealed sender messages (96-bit)
//! - **Identity Binding**: Identity key binds subsequent messages to this user

use base32::Alphabet;

use super::UserId;

/// Private friend code containing all material for anonymous contact.
#[derive(Clone)]
pub struct PrivateFriendCode {
    /// Target user's ID.
    pub user_id: UserId,
    /// Target user's Signal Protocol identity key (33 bytes).
    pub identity_key: [u8; 33],
    /// Target user's delivery token (12 bytes / 96 bits).
    pub delivery_token: [u8; 12],
}

/// Error parsing a private friend code.
#[derive(Debug, thiserror::Error)]
pub enum PrivateFriendCodeError {
    /// Invalid URL format.
    #[error("invalid URL format: {0}")]
    InvalidFormat(String),

    /// Base32 decoding failed.
    #[error("base32 decode failed: {0}")]
    Base32DecodeFailed(String),

    /// Invalid data length.
    #[error("invalid data length: expected 61 bytes, got {0}")]
    InvalidLength(usize),

    /// Invalid identity key format.
    #[error("invalid identity key: {0}")]
    InvalidIdentityKey(String),

    /// Missing profile key for generation.
    #[error("profile key required for code generation")]
    MissingProfileKey,

    /// Missing identity key for generation.
    #[error("identity key required for code generation")]
    MissingIdentityKey,
}

impl PrivateFriendCode {
    /// Total size of the encoded data.
    pub const DATA_SIZE: usize = 16 + 33 + 12; // 61 bytes

    /// URL prefix for private friend codes.
    pub const URL_PREFIX: &'static str = "chatter://private/";

    /// Create a new private friend code.
    pub fn new(user_id: UserId, identity_key: [u8; 33], delivery_token: [u8; 12]) -> Self {
        Self {
            user_id,
            identity_key,
            delivery_token,
        }
    }

    /// Derive delivery token from profile key (thin wrapper around
    /// [`crate::crypto::auth_crypto::derive_delivery_token`]).
    pub fn derive_delivery_token(profile_key: &[u8; 32]) -> [u8; 12] {
        crate::crypto::auth_crypto::derive_delivery_token(profile_key)
    }

    /// Encode to bytes.
    pub fn to_bytes(&self) -> [u8; Self::DATA_SIZE] {
        let mut bytes = [0u8; Self::DATA_SIZE];
        bytes[..16].copy_from_slice(self.user_id.as_bytes());
        bytes[16..49].copy_from_slice(&self.identity_key);
        bytes[49..61].copy_from_slice(&self.delivery_token);
        bytes
    }

    /// Parse from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PrivateFriendCodeError> {
        if bytes.len() != Self::DATA_SIZE {
            return Err(PrivateFriendCodeError::InvalidLength(bytes.len()));
        }

        // Parse user ID (16 bytes UUID)
        let user_id = UserId::try_from_slice(&bytes[..16])
            .ok_or_else(|| PrivateFriendCodeError::InvalidFormat("invalid user ID".into()))?;

        // Parse identity key (33 bytes)
        let mut identity_key = [0u8; 33];
        identity_key.copy_from_slice(&bytes[16..49]);

        // Verify identity key format
        if identity_key[0] != 0x05 {
            return Err(PrivateFriendCodeError::InvalidIdentityKey(format!(
                "expected type byte 0x05, got 0x{:02x}",
                identity_key[0]
            )));
        }

        // Parse delivery token (12 bytes)
        let mut delivery_token = [0u8; 12];
        delivery_token.copy_from_slice(&bytes[49..61]);

        Ok(Self {
            user_id,
            identity_key,
            delivery_token,
        })
    }

    /// Encode to base32 string.
    pub fn to_base32(&self) -> String {
        base32::encode(Alphabet::Rfc4648 { padding: false }, &self.to_bytes())
    }

    /// Parse from base32 string.
    pub fn from_base32(s: &str) -> Result<Self, PrivateFriendCodeError> {
        let bytes = base32::decode(Alphabet::Rfc4648 { padding: false }, s)
            .ok_or_else(|| PrivateFriendCodeError::Base32DecodeFailed("invalid base32".into()))?;

        Self::from_bytes(&bytes)
    }

    /// Encode to URL format.
    ///
    /// Format: `chatter://private/{base32_data}`
    pub fn to_url(&self) -> String {
        format!("{}{}", Self::URL_PREFIX, self.to_base32())
    }

    /// Parse from URL or raw base32 string.
    ///
    /// Accepts:
    /// - `chatter://private/{base32}` (URL format)
    /// - `{base32}` (raw base32)
    pub fn parse(input: &str) -> Result<Self, PrivateFriendCodeError> {
        let input = input.trim();

        // Try URL format first
        if let Some(base32_part) = input.strip_prefix(Self::URL_PREFIX) {
            return Self::from_base32(base32_part);
        }

        // Try raw base32
        Self::from_base32(input)
    }

    /// Get the X25519 public key from the identity key.
    ///
    /// Strips the 0x05 type byte to get the raw 32-byte Curve25519 point.
    pub fn x25519_public_key(&self) -> [u8; 32] {
        let mut key = [0u8; 32];
        key.copy_from_slice(&self.identity_key[1..33]);
        key
    }

    /// Check if this is a valid private friend code URL.
    pub fn is_private_code(input: &str) -> bool {
        input.trim().starts_with(Self::URL_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let user_id = UserId::new();
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x05; // Type byte
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut identity_key[1..])
                .expect("OS CSPRNG must not fail");
        }

        let mut delivery_token = [0u8; 12];
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut delivery_token)
                .expect("OS CSPRNG must not fail");
        }

        let code = PrivateFriendCode::new(user_id, identity_key, delivery_token);

        // Test bytes roundtrip
        let bytes = code.to_bytes();
        let parsed = PrivateFriendCode::from_bytes(&bytes).unwrap();
        assert_eq!(code.user_id, parsed.user_id);
        assert_eq!(code.identity_key, parsed.identity_key);
        assert_eq!(code.delivery_token, parsed.delivery_token);

        // Test base32 roundtrip
        let base32 = code.to_base32();
        let parsed = PrivateFriendCode::from_base32(&base32).unwrap();
        assert_eq!(code.user_id, parsed.user_id);

        // Test URL roundtrip
        let url = code.to_url();
        assert!(url.starts_with("chatter://private/"));
        let parsed = PrivateFriendCode::parse(&url).unwrap();
        assert_eq!(code.user_id, parsed.user_id);
    }

    #[test]
    fn test_delivery_token_derivation() {
        let mut profile_key = [0u8; 32];
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut profile_key)
                .expect("OS CSPRNG must not fail");
        }

        let token1 = PrivateFriendCode::derive_delivery_token(&profile_key);
        let token2 = PrivateFriendCode::derive_delivery_token(&profile_key);

        // Same input should produce same token
        assert_eq!(token1, token2);
        assert_eq!(token1.len(), 12);
    }

    #[test]
    fn test_invalid_identity_key() {
        let user_id = UserId::new();
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x07; // Wrong type byte

        let delivery_token = [0u8; 12];

        let code = PrivateFriendCode {
            user_id,
            identity_key,
            delivery_token,
        };

        let bytes = code.to_bytes();
        let result = PrivateFriendCode::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_url_format() {
        let user_id = UserId::new();
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x05;
        let delivery_token = [0u8; 12];

        let code = PrivateFriendCode::new(user_id, identity_key, delivery_token);
        let url = code.to_url();

        assert!(PrivateFriendCode::is_private_code(&url));
        assert!(!PrivateFriendCode::is_private_code(
            "chatter://connect/user-id"
        ));
    }

    #[test]
    fn test_x25519_extraction() {
        let user_id = UserId::new();
        let mut identity_key = [0u8; 33];
        identity_key[0] = 0x05;
        for i in 1..33 {
            identity_key[i] = i as u8;
        }
        let delivery_token = [0u8; 12];

        let code = PrivateFriendCode::new(user_id, identity_key, delivery_token);
        let x25519 = code.x25519_public_key();

        assert_eq!(x25519.len(), 32);
        for i in 0..32 {
            assert_eq!(x25519[i], (i + 1) as u8);
        }
    }
}
