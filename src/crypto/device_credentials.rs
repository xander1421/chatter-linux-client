//! Device key management.
//!
//! Device credential generation and storage for Signal-style device authentication.
//! Implements device auth token generation and UAK (Unidentified Access Key) derivation.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::CryptoError;

/// UAK HKDF info string per Signal specification.
const UAK_HKDF_INFO: &[u8] = b"UnidentifiedDelivery";

/// Device authentication token (32 bytes).
///
/// Random token used for HTTP Basic authentication with the server.
/// Format: `{user_id}.{device_id}:{token_base64}` in Authorization header.
#[derive(Debug, Clone)]
pub struct DeviceToken {
    /// 32-byte random authentication token.
    token: [u8; 32],
}

impl DeviceToken {
    /// Generate a new random device token.
    pub fn generate() -> Self {
        let mut token = [0u8; 32];
        rand::rng().fill_bytes(&mut token);
        Self { token }
    }

    /// Reconstruct token from stored bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != 32 {
            return Err(CryptoError::InvalidKeyFormat);
        }
        let mut token = [0u8; 32];
        token.copy_from_slice(bytes);
        Ok(Self { token })
    }

    /// Get the raw token bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.token
    }

    /// Encode token as base64 for transmission.
    pub fn to_base64(&self) -> String {
        BASE64.encode(&self.token)
    }
}

/// Device credentials for Signal-style authentication.
///
/// Holds:
/// - Device ID (1-127 per Signal spec)
/// - Auth token (32-byte random for HTTP Basic auth)
/// - UAK (16-byte key for sealed sender)
///
/// Used by AuthInterceptor to construct Authorization headers.
#[derive(Debug, Clone)]
pub struct DeviceCredentials {
    /// Signal device ID (1 for primary device, 2-127 for linked devices).
    device_id: u32,
    /// Random device authentication token.
    auth_token: DeviceToken,
    /// Unidentified Access Key for sealed sender.
    unidentified_access_key: [u8; 16],
    /// Whether to allow unrestricted unidentified access.
    unrestricted_unidentified_access: bool,
}

impl DeviceCredentials {
    /// Generate new device credentials.
    ///
    /// # Arguments
    /// * `device_id` - Signal device ID (1-127)
    /// * `profile_key` - 32-byte profile key for UAK derivation
    ///
    /// # Returns
    /// New DeviceCredentials with random auth token and derived UAK.
    pub fn generate(device_id: u32, profile_key: &[u8; 32]) -> Self {
        let auth_token = DeviceToken::generate();
        let unidentified_access_key = derive_uak(profile_key);

        Self {
            device_id,
            auth_token,
            unidentified_access_key,
            unrestricted_unidentified_access: false,
        }
    }

    /// Reconstruct credentials from stored values.
    ///
    /// Used after login when server provides stored credentials.
    pub fn from_stored(
        device_id: u32,
        auth_token: [u8; 32],
        unidentified_access_key: [u8; 16],
    ) -> Self {
        Self {
            device_id,
            auth_token: DeviceToken { token: auth_token },
            unidentified_access_key,
            unrestricted_unidentified_access: false,
        }
    }

    /// Get the device ID.
    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    /// Get the auth token.
    pub fn auth_token(&self) -> &DeviceToken {
        &self.auth_token
    }

    /// Get the UAK.
    pub fn unidentified_access_key(&self) -> &[u8; 16] {
        &self.unidentified_access_key
    }

    /// Format username for HTTP Basic auth.
    ///
    /// Returns `{user_id}.{device_id}` format per Signal spec.
    pub fn auth_username(&self, user_id: &str) -> String {
        format!("{}.{}", user_id, self.device_id)
    }

    /// Get base64-encoded auth token for HTTP Basic auth password.
    pub fn auth_token_base64(&self) -> String {
        self.auth_token.to_base64()
    }

    /// Verify a received UAK against our stored UAK.
    ///
    /// Uses constant-time comparison to prevent timing attacks.
    pub fn verify_uak(&self, received_uak: &[u8]) -> bool {
        if received_uak.len() != 16 {
            return false;
        }
        self.unidentified_access_key.ct_eq(received_uak).into()
    }

    /// Check if unrestricted unidentified access is allowed.
    ///
    /// When enabled, any UAK is accepted (don't leak user existence).
    /// This is a user privacy setting.
    pub fn allows_unrestricted_unidentified(&self) -> bool {
        self.unrestricted_unidentified_access
    }

    /// Set unrestricted unidentified access.
    pub fn set_unrestricted_unidentified_access(&mut self, allow: bool) {
        self.unrestricted_unidentified_access = allow;
    }
}

/// Derive Unidentified Access Key from profile key.
///
/// Uses HKDF-SHA256 with info="UnidentifiedDelivery" per Signal spec.
///
/// # Arguments
/// * `profile_key` - 32-byte profile key
///
/// # Returns
/// 16-byte UAK
pub fn derive_uak(profile_key: &[u8; 32]) -> [u8; 16] {
    // HKDF-SHA256: salt=None, ikm=profile_key, info="UnidentifiedDelivery"
    let hkdf = Hkdf::<Sha256>::new(None, profile_key);
    let mut uak = [0u8; 16];
    // Extract exactly 16 bytes for UAK
    hkdf.expand(UAK_HKDF_INFO, &mut uak)
        .expect("HKDF expansion for 16 bytes should never fail");
    uak
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_token_generation() {
        let token = DeviceToken::generate();
        let bytes = token.as_bytes();

        // Verify 32 bytes
        assert_eq!(bytes.len(), 32);

        // Verify not all zeros (overwhelmingly likely)
        assert_ne!(bytes, &[0u8; 32]);
    }

    #[test]
    fn test_device_token_roundtrip() {
        let token = DeviceToken::generate();
        let bytes = token.as_bytes().to_vec();

        let restored = DeviceToken::from_bytes(&bytes).expect("should restore from bytes");
        assert_eq!(restored.as_bytes(), token.as_bytes());
    }

    #[test]
    fn test_device_token_invalid_length() {
        // Too short
        let result = DeviceToken::from_bytes(&[0u8; 16]);
        assert!(result.is_err());

        // Too long
        let result = DeviceToken::from_bytes(&[0u8; 64]);
        assert!(result.is_err());
    }

    #[test]
    fn test_device_credentials_generation() {
        let device_id = 1u32;
        let profile_key = [0x42u8; 32];

        let creds = DeviceCredentials::generate(device_id, &profile_key);

        assert_eq!(creds.device_id(), device_id);
        assert_eq!(creds.auth_token().as_bytes().len(), 32);
        assert_eq!(creds.unidentified_access_key().len(), 16);
        assert!(!creds.allows_unrestricted_unidentified());
    }

    #[test]
    fn test_auth_username_format() {
        let profile_key = [0x42u8; 32];
        let creds = DeviceCredentials::generate(1, &profile_key);

        let username = creds.auth_username("d04b0053-6557-48f9-86c2-e8ca389279bd");
        assert_eq!(username, "d04b0053-6557-48f9-86c2-e8ca389279bd.1");

        // Test with device ID 5
        let creds2 = DeviceCredentials::generate(5, &profile_key);
        let username2 = creds2.auth_username("abc123");
        assert_eq!(username2, "abc123.5");
    }

    #[test]
    fn test_uak_derivation_deterministic() {
        let profile_key = [0x42u8; 32];

        let uak1 = derive_uak(&profile_key);
        let uak2 = derive_uak(&profile_key);

        // Same profile key should produce same UAK
        assert_eq!(uak1, uak2);
    }

    #[test]
    fn test_uak_derivation_different_keys() {
        let profile_key1 = [0x42u8; 32];
        let profile_key2 = [0x43u8; 32];

        let uak1 = derive_uak(&profile_key1);
        let uak2 = derive_uak(&profile_key2);

        // Different profile keys should produce different UAKs
        assert_ne!(uak1, uak2);
    }

    #[test]
    fn test_uak_verification_constant_time() {
        let profile_key = [0x42u8; 32];
        let creds = DeviceCredentials::generate(1, &profile_key);

        // Correct UAK should verify
        let correct_uak = derive_uak(&profile_key);
        assert!(creds.verify_uak(&correct_uak));

        // Incorrect UAK should not verify
        let incorrect_uak = [0xFFu8; 16];
        assert!(!creds.verify_uak(&incorrect_uak));

        // Wrong length should not verify
        let wrong_length = [0x42u8; 8];
        assert!(!creds.verify_uak(&wrong_length));
    }

    #[test]
    fn test_from_stored() {
        let device_id = 3u32;
        let auth_token = [0xABu8; 32];
        let uak = [0xCDu8; 16];

        let creds = DeviceCredentials::from_stored(device_id, auth_token, uak);

        assert_eq!(creds.device_id(), device_id);
        assert_eq!(creds.auth_token().as_bytes(), &auth_token);
        assert_eq!(creds.unidentified_access_key(), &uak);
    }

    #[test]
    fn test_auth_token_base64() {
        let token = DeviceToken::from_bytes(&[0u8; 32]).unwrap();
        let b64 = token.to_base64();

        // 32 bytes -> 44 chars base64 (with padding)
        assert_eq!(b64.len(), 44);
        assert_eq!(b64, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn test_unrestricted_access_setting() {
        let profile_key = [0x42u8; 32];
        let mut creds = DeviceCredentials::generate(1, &profile_key);

        assert!(!creds.allows_unrestricted_unidentified());

        creds.set_unrestricted_unidentified_access(true);
        assert!(creds.allows_unrestricted_unidentified());

        creds.set_unrestricted_unidentified_access(false);
        assert!(!creds.allows_unrestricted_unidentified());
    }
}
