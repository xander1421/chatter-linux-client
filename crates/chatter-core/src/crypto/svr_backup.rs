//! SVR Backup Encryption/Decryption.
//!
//! Encrypts and decrypts key backup payloads for Secure Value Recovery.
//! Uses AES-256-GCM for confidentiality and HKDF-SHA256 for the backup ID.
//! Both keys are per-use HKDF expansions of the SVR secret, never the secret
//! itself — see `svr::svr_subkey` (CRYPTO-3).
//!
//! Wire format for serialized payload (fixed size):
//!   version(1) || identity_len(4 LE) || identity_keypair(64) || profile_len(4 LE) || profile_key(32)
//!   || username_len(4 LE) || full_username, zero-padded to 64 bytes
//!
//! The username field is padded to a fixed width so the ciphertext length the
//! server stores does not reveal the handle's length.
//!
//! Wire format for encrypted backup:
//!   nonce(12) || ciphertext || tag(16)

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use zeroize::Zeroize;

use crate::crypto::svr::svr_subkey;
use crate::error::CryptoError;

/// HKDF info strings for the two consumers of the SVR secret in this module.
/// See `svr::svr_subkey` (CRYPTO-3) for why they are not the raw secret.
const SVR_INFO_BACKUP_AES: &[u8] = b"Chatter SVR Backup AES-GCM Key v1";
const SVR_INFO_BACKUP_ID: &[u8] = b"Chatter SVR Backup Id v1";

/// Current serialization version for key backup payloads. 0x02 added the
/// full username; 0x01 backups are rejected (pre-release clean break).
const BACKUP_VERSION: u8 = 0x02;

/// AES-256-GCM nonce size in bytes.
const NONCE_SIZE: usize = 12;

/// Expected identity keypair size: 32 bytes private + 32 bytes public.
const IDENTITY_KEYPAIR_SIZE: usize = 64;

/// Expected profile key size.
const PROFILE_KEY_SIZE: usize = 32;

/// Fixed width of the padded username field. A handle is a nickname of at
/// most 32 characters (`auth_crypto::username_candidates`), '.', and a
/// numeric discriminator, so 64 bytes holds every one with room to spare.
const USERNAME_FIELD_SIZE: usize = 64;

/// Serialized payload size — every payload is exactly this long:
/// version(1) + len(4) + identity(64) + len(4) + profile(32) + len(4) + username(64).
const SERIALIZED_SIZE: usize =
    1 + 4 + IDENTITY_KEYPAIR_SIZE + 4 + PROFILE_KEY_SIZE + 4 + USERNAME_FIELD_SIZE;

/// Key material that must be backed up for account recovery.
#[derive(Clone, Zeroize)]
#[zeroize(drop)]
pub struct KeyBackupPayload {
    /// Signal identity keypair: 32-byte private key || 32-byte public key.
    pub identity_keypair: [u8; 64],
    /// 32-byte profile encryption key.
    pub profile_key: [u8; 32],
    /// The account's `nickname.discriminator` handle (`keys::FULL_USERNAME`).
    /// It is chosen at registration and exists nowhere else on a reinstalled
    /// device, so without it a restored account has no handle to share.
    pub full_username: String,
}

impl KeyBackupPayload {
    /// Serialize the payload into the versioned, fixed-size wire format.
    ///
    /// Fails if `full_username` is empty or longer than the padded field.
    pub fn serialize(&self) -> Result<Vec<u8>, CryptoError> {
        let username = self.full_username.as_bytes();
        if username.is_empty() || username.len() > USERNAME_FIELD_SIZE {
            return Err(CryptoError::Validation(format!(
                "backup username must be 1..={USERNAME_FIELD_SIZE} bytes, got {}",
                username.len(),
            )));
        }

        let mut buf = Vec::with_capacity(SERIALIZED_SIZE);

        // Version byte
        buf.push(BACKUP_VERSION);

        // Identity keypair: length prefix (4 bytes LE) + data
        buf.extend_from_slice(&(IDENTITY_KEYPAIR_SIZE as u32).to_le_bytes());
        buf.extend_from_slice(&self.identity_keypair);

        // Profile key: length prefix (4 bytes LE) + data
        buf.extend_from_slice(&(PROFILE_KEY_SIZE as u32).to_le_bytes());
        buf.extend_from_slice(&self.profile_key);

        // Username: length prefix (4 bytes LE) + data, zero-padded
        buf.extend_from_slice(&(username.len() as u32).to_le_bytes());
        buf.extend_from_slice(username);
        buf.resize(SERIALIZED_SIZE, 0);

        Ok(buf)
    }

    /// Deserialize a payload from the versioned wire format.
    ///
    /// Validates version byte, total size, field lengths and padding.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != SERIALIZED_SIZE {
            return Err(CryptoError::DecryptionFailed(format!(
                "backup payload is {} bytes, expected {}",
                bytes.len(),
                SERIALIZED_SIZE,
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
        let identity_len =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().map_err(|_| {
                CryptoError::DecryptionFailed("invalid identity length field".into())
            })?) as usize;
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

        let profile_len =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().map_err(|_| {
                CryptoError::DecryptionFailed("invalid profile length field".into())
            })?) as usize;
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
        offset += PROFILE_KEY_SIZE;

        // Username (total size is fixed and checked above, so the length
        // field and the padded field are both in bounds)
        let username_len =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().map_err(|_| {
                CryptoError::DecryptionFailed("invalid username length field".into())
            })?) as usize;
        offset += 4;

        if username_len == 0 || username_len > USERNAME_FIELD_SIZE {
            return Err(CryptoError::Validation(format!(
                "backup username length {username_len} out of range 1..={USERNAME_FIELD_SIZE}",
            )));
        }
        let (username, padding) = bytes[offset..].split_at(username_len);
        if padding.iter().any(|&b| b != 0) {
            return Err(CryptoError::Validation(
                "backup username padding is not zero".into(),
            ));
        }
        let full_username = std::str::from_utf8(username)
            .map_err(|_| CryptoError::Validation("backup username is not UTF-8".into()))?
            .to_owned();

        Ok(Self {
            identity_keypair,
            profile_key,
            full_username,
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
    let mut aes_key = svr_subkey(svr_secret, SVR_INFO_BACKUP_AES);
    let cipher = Aes256Gcm::new_from_slice(&aes_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    aes_key.zeroize();

    // Generate random 12-byte nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    {
        use rand::TryRngCore;
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .expect("OS CSPRNG must not fail");
    }
    let nonce = <&Nonce<_>>::try_from(&nonce_bytes[..]).expect("fixed cryptographic input length");

    // Serialize then encrypt
    let mut plaintext = payload.serialize()?;
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
    let nonce = <&Nonce<_>>::try_from(&nonce_bytes[..]).expect("fixed cryptographic input length");

    let mut aes_key = svr_subkey(svr_secret, SVR_INFO_BACKUP_AES);
    let cipher = Aes256Gcm::new_from_slice(&aes_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    aes_key.zeroize();

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
/// HKDF-SHA256(svr_secret, info = "Chatter SVR Backup Id v1") — a 32-byte
/// identifier the server can use to locate the encrypted backup without learning
/// the secret, and which shares no key material with the AES-GCM key that
/// protects the backup itself.
pub fn derive_backup_id(svr_secret: &[u8; 32]) -> [u8; 32] {
    svr_subkey(svr_secret, SVR_INFO_BACKUP_ID)
}
