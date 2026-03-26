//! SVR Backup Encryption/Decryption.
//!
//! Encrypts and decrypts key backup payloads for Secure Value Recovery.
//! Uses AES-256-GCM for confidentiality and HMAC-SHA256 for backup ID derivation.
//!
//! Wire format for serialized payload:
//!   version(1) || identity_len(4 LE) || identity_keypair(64) || profile_len(4 LE) || profile_key(32)
//!
//! Wire format for encrypted backup:
//!   nonce(12) || ciphertext || tag(16)

use zeroize::Zeroize;

use crate::error::CryptoError;

/// Current serialization version for key backup payloads.
const BACKUP_VERSION: u8 = 0x01;

/// AES-256-GCM nonce size in bytes.
const NONCE_SIZE: usize = 12;

/// Expected identity keypair size: 32 bytes private + 32 bytes public.
const IDENTITY_KEYPAIR_SIZE: usize = 64;

/// Expected profile key size.
const PROFILE_KEY_SIZE: usize = 32;

/// Key material that must be backed up for account recovery.
#[derive(Clone, Zeroize)]
#[zeroize(drop)]
pub struct KeyBackupPayload {
    /// Signal identity keypair: 32-byte private key || 32-byte public key.
    pub identity_keypair: [u8; 64],
    /// 32-byte profile encryption key.
    pub profile_key: [u8; 32],
}

impl KeyBackupPayload {
    /// Serialize the payload into a versioned wire format.
    ///
    /// Format: version(1) || identity_len(4 LE) || identity_keypair(64) || profile_len(4 LE) || profile_key(32)
    pub fn serialize(&self) -> Vec<u8> {
        todo!()
    }

    /// Deserialize a payload from the versioned wire format.
    ///
    /// Validates version byte and field lengths.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, CryptoError> {
        let _ = bytes;
        todo!()
    }
}

/// Encrypt a key backup payload using AES-256-GCM.
///
/// Returns: nonce(12) || ciphertext || tag(16)
pub fn encrypt_backup(payload: &KeyBackupPayload, svr_secret: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    let _ = (payload, svr_secret);
    todo!()
}

/// Decrypt a key backup payload previously encrypted with `encrypt_backup`.
///
/// Expects: nonce(12) || ciphertext || tag(16)
pub fn decrypt_backup(encrypted: &[u8], svr_secret: &[u8; 32]) -> Result<KeyBackupPayload, CryptoError> {
    let _ = (encrypted, svr_secret);
    todo!()
}

/// Derive a deterministic backup ID from an SVR secret.
///
/// Uses HMAC-SHA256(svr_secret, "BackupId") to produce a 32-byte identifier
/// that the server can use to locate the encrypted backup without learning the secret.
pub fn derive_backup_id(svr_secret: &[u8; 32]) -> [u8; 32] {
    let _ = svr_secret;
    todo!()
}
