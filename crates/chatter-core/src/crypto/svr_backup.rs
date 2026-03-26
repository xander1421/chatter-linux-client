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

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;
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

/// Minimum serialized payload size: version(1) + len(4) + identity(64) + len(4) + profile(32).
const MIN_SERIALIZED_SIZE: usize = 1 + 4 + IDENTITY_KEYPAIR_SIZE + 4 + PROFILE_KEY_SIZE;

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
        let mut buf = Vec::with_capacity(MIN_SERIALIZED_SIZE);

        // Version byte
        buf.push(BACKUP_VERSION);

        // Identity keypair: length prefix (4 bytes LE) + data
        buf.extend_from_slice(&(IDENTITY_KEYPAIR_SIZE as u32).to_le_bytes());
        buf.extend_from_slice(&self.identity_keypair);

        // Profile key: length prefix (4 bytes LE) + data
        buf.extend_from_slice(&(PROFILE_KEY_SIZE as u32).to_le_bytes());
        buf.extend_from_slice(&self.profile_key);

        buf
    }

    /// Deserialize a payload from the versioned wire format.
    ///
    /// Validates version byte and field lengths.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() < MIN_SERIALIZED_SIZE {
            return Err(CryptoError::DecryptionFailed(format!(
                "backup payload too short: {} bytes, need at least {}",
                bytes.len(),
                MIN_SERIALIZED_SIZE,
            )));
        }

        let mut offset = 0;

        // Version check
        let version = bytes[offset];
        offset += 1;
        if version != BACKUP_VERSION {
            return Err(CryptoError::Validation(format!(
                "unsupported backup version: 0x{version:02X}, expected 0x{BACKUP_VERSION:02X}",
            )));
        }

        // Identity keypair
        let identity_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .map_err(|_| CryptoError::DecryptionFailed("invalid identity length field".into()))?,
        ) as usize;
        offset += 4;

        if identity_len != IDENTITY_KEYPAIR_SIZE {
            return Err(CryptoError::InvalidKeyLength {
                expected: IDENTITY_KEYPAIR_SIZE,
                got: identity_len,
            });
        }

        if offset + identity_len > bytes.len() {
            return Err(CryptoError::DecryptionFailed(
                "identity keypair data truncated".into(),
            ));
        }

        let mut identity_keypair = [0u8; 64];
        identity_keypair.copy_from_slice(&bytes[offset..offset + IDENTITY_KEYPAIR_SIZE]);
        offset += IDENTITY_KEYPAIR_SIZE;

        // Profile key
        if offset + 4 > bytes.len() {
            return Err(CryptoError::DecryptionFailed(
                "profile key length field truncated".into(),
            ));
        }

        let profile_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .map_err(|_| CryptoError::DecryptionFailed("invalid profile length field".into()))?,
        ) as usize;
        offset += 4;

        if profile_len != PROFILE_KEY_SIZE {
            return Err(CryptoError::InvalidKeyLength {
                expected: PROFILE_KEY_SIZE,
                got: profile_len,
            });
        }

        if offset + profile_len > bytes.len() {
            return Err(CryptoError::DecryptionFailed(
                "profile key data truncated".into(),
            ));
        }

        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&bytes[offset..offset + PROFILE_KEY_SIZE]);

        Ok(Self {
            identity_keypair,
            profile_key,
        })
    }
}

/// Encrypt a key backup payload using AES-256-GCM.
///
/// Returns: nonce(12) || ciphertext || tag(16)
pub fn encrypt_backup(
    payload: &KeyBackupPayload,
    svr_secret: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(svr_secret)
        .map_err(|_| CryptoError::InvalidKeyFormat)?;

    // Generate random 12-byte nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Serialize then encrypt
    let mut plaintext = payload.serialize();
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|e| CryptoError::EncryptionFailed(format!("AES-GCM encrypt failed: {e}")))?;

    // Zeroize the plaintext intermediate
    plaintext.zeroize();

    // Assemble: nonce || ciphertext (includes tag)
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Decrypt a key backup payload previously encrypted with `encrypt_backup`.
///
/// Expects: nonce(12) || ciphertext || tag(16)
pub fn decrypt_backup(
    encrypted: &[u8],
    svr_secret: &[u8; 32],
) -> Result<KeyBackupPayload, CryptoError> {
    if encrypted.len() < NONCE_SIZE + 16 {
        return Err(CryptoError::DecryptionFailed(format!(
            "encrypted backup too short: {} bytes",
            encrypted.len(),
        )));
    }

    let (nonce_bytes, ciphertext) = encrypted.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(svr_secret)
        .map_err(|_| CryptoError::InvalidKeyFormat)?;

    let mut plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(format!("AES-GCM decrypt failed: {e}")))?;

    let result = KeyBackupPayload::deserialize(&plaintext);

    // Zeroize the plaintext intermediate regardless of deserialization outcome
    plaintext.zeroize();

    result
}

/// Derive a deterministic backup ID from an SVR secret.
///
/// Uses HMAC-SHA256(svr_secret, "BackupId") to produce a 32-byte identifier
/// that the server can use to locate the encrypted backup without learning the secret.
pub fn derive_backup_id(svr_secret: &[u8; 32]) -> [u8; 32] {
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = <HmacSha256 as Mac>::new_from_slice(svr_secret)
        .expect("HMAC can take key of any size");
    mac.update(b"BackupId");

    let result = mac.finalize();
    let mut id = [0u8; 32];
    id.copy_from_slice(&result.into_bytes());
    id
}
