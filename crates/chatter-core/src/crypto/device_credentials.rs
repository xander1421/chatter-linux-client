//! Device key management.
//!
//! Device credential generation and storage for Signal-style device authentication.
//! Implements device auth token generation and UAK (Unidentified Access Key) derivation.

use std::fmt;

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::error::CryptoError;

/// Device authentication token (32 bytes).
///
/// Random token used for HTTP Basic authentication with the server.
/// Format: `{user_id}.{device_id}:{token_base64}` in Authorization header.
///
/// The token bytes are zeroized when the struct is dropped, and `Debug` is
/// redacted so accidental `{:?}` formatting (panics, log statements) cannot
/// leak the HTTP Basic password.
#[derive(Clone)]
pub struct DeviceToken {
    /// 32-byte random authentication token.
    token: [u8; 32],
}

impl fmt::Debug for DeviceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceToken")
            .field("token", &"[REDACTED 32 bytes]")
            .finish()
    }
}

impl Drop for DeviceToken {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl DeviceToken {
    /// Generate a new random device token.
    pub fn generate() -> Self {
        let mut token = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut token).expect("OS CSPRNG must not fail");}
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
        BASE64.encode(self.token)
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
///
/// The UAK is zeroized on drop; the auth token is zeroized via `DeviceToken`'s
/// own `Drop`. `Debug` is redacted to keep the token and UAK out of any
/// accidental `{:?}` output.
#[derive(Clone)]
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

impl fmt::Debug for DeviceCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCredentials")
            .field("device_id", &self.device_id)
            .field("auth_token", &"[REDACTED]")
            .field("unidentified_access_key", &"[REDACTED 16 bytes]")
            .field(
                "unrestricted_unidentified_access",
                &self.unrestricted_unidentified_access,
            )
            .finish()
    }
}

impl Drop for DeviceCredentials {
    fn drop(&mut self) {
        self.unidentified_access_key.zeroize();
    }
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
/// Uses AES-256-GCM with zero nonce encrypting zero plaintext per libsignal's
/// `ProfileKey.deriveAccessKey()` implementation.
///
/// Technical note: AES-GCM = CTR mode + auth tag. Taking first 16 bytes of
/// ciphertext (before auth tag) is equivalent to AES-CTR, which is what
/// libsignal actually uses internally.
///
/// # Arguments
/// * `profile_key` - 32-byte profile key (used as AES-256 key)
///
/// # Returns
/// 16-byte UAK (first 16 bytes of AES-GCM ciphertext)
pub fn derive_uak(profile_key: &[u8; 32]) -> [u8; 16] {
    // Create AES-256-GCM cipher with profile key
    let cipher = Aes256Gcm::new_from_slice(profile_key)
        .expect("Profile key is always 32 bytes");

    // Use 12-byte zero nonce (Signal's standard for UAK derivation)
    let nonce = Nonce::from_slice(&[0u8; 12]);

    // Encrypt 16 zero bytes to derive UAK
    let plaintext = [0u8; 16];
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .expect("AES-GCM encryption of zeros should never fail");

    // First 16 bytes of ciphertext is the UAK (auth tag follows but we ignore it)
    let mut uak = [0u8; 16];
    uak.copy_from_slice(&ciphertext[..16]);
    uak
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libsignal's known-answer vectors for `ProfileKey::derive_access_key`
    /// (rust/zkgroup/src/api/profiles/profile_key.rs `access_key_kat`, rev
    /// 8fc2113). A mismatch means our UAK is not the one a Signal-style server
    /// would derive from the same profile key.
    #[test]
    fn derive_uak_matches_libsignal_access_key_kat() {
        let kat: [([u8; 32], [u8; 16]); 2] = [
            (
                [
                    0xb9, 0x50, 0x42, 0xa2, 0xc2, 0xd9, 0xe5, 0xb3, 0xbb, 0x9, 0x30, 0xe, 0xe4,
                    0x8, 0xa1, 0x72, 0xfa, 0xcd, 0x96, 0xe9, 0x1b, 0x50, 0x4e, 0x4, 0x3a, 0x5a,
                    0x2, 0x3d, 0xc4, 0xcf, 0xf3, 0x59,
                ],
                [
                    0x24, 0xfb, 0x96, 0xd4, 0xa5, 0xe3, 0x33, 0xe9, 0xd4, 0x45, 0x12, 0x5, 0xb9,
                    0xe2, 0xfa, 0xed,
                ],
            ),
            (
                [
                    0x26, 0x19, 0x7b, 0x17, 0xe5, 0xa2, 0xc3, 0x6d, 0x8c, 0x95, 0x18, 0xc3, 0x53,
                    0x58, 0xf1, 0x23, 0xc4, 0x76, 0x0, 0xd, 0xb6, 0xda, 0x75, 0x65, 0xc0, 0xd4,
                    0x1f, 0x66, 0x74, 0x46, 0x2c, 0x4d,
                ],
                [
                    0xe8, 0x95, 0xc3, 0xc, 0xf7, 0x80, 0x75, 0x7d, 0x22, 0xf7, 0xa1, 0x79, 0x70,
                    0x8b, 0x14, 0xa1,
                ],
            ),
        ];
        for (profile_key, access_key) in kat {
            assert_eq!(derive_uak(&profile_key), access_key);
        }
    }

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

    #[test]
    fn test_device_token_debug_redacted() {
        let token = DeviceToken::from_bytes(&[0xABu8; 32]).unwrap();
        let dbg = format!("{:?}", token);
        assert!(dbg.contains("REDACTED"), "debug output: {dbg}");
        assert!(!dbg.contains("ab"), "raw bytes leaked in debug: {dbg}");
        assert!(!dbg.contains("AB"), "raw bytes leaked in debug: {dbg}");
    }

    #[test]
    fn test_device_credentials_debug_redacted() {
        let creds = DeviceCredentials::from_stored(7, [0xABu8; 32], [0xCDu8; 16]);
        let dbg = format!("{:?}", creds);
        assert!(dbg.contains("REDACTED"), "debug output: {dbg}");
        assert!(dbg.contains("device_id"), "device_id should still be visible: {dbg}");
        assert!(!dbg.contains("ab"), "auth_token bytes leaked: {dbg}");
        assert!(!dbg.contains("cd"), "uak bytes leaked: {dbg}");
    }
}
