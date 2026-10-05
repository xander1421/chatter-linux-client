//! Message-history backup file crypto (L7 message-backup).
//!
//! Chatter-native versioned container for the encrypted message-history
//! export. Format discipline follows libsignal's `message-backup` crate
//! (cleartext key-agility header, everything else AEAD-sealed, payload is a
//! length-delimited frame stream) but the schema is chatter's own — Signal's
//! backup data model does not fit this app.
//!
//! # File layout
//!
//! ```text
//! magic "CHTRBKP1" (8 bytes)
//! header_len (u32 LE)
//! header (prost BackupFileHeader, CLEARTEXT — must be readable before any
//!         key exists: it carries the Argon2 salt / key mode needed to derive
//!         the KEK, plus the wrapped file key)
//! ciphertext = AES-256-GCM(file_key, payload_nonce,
//!                          aad = exact header bytes) over the payload
//! ```
//!
//! The payload plaintext is a stream of length-delimited prost `Frame`s (see
//! `storage::backup_data`). Binding the header as AAD means any tamper with
//! the salt, mode, or wrapped key makes decryption fail — there is no
//! unauthenticated byte in the file after the magic.
//!
//! # Keys
//!
//! - A random 32-byte **file key** encrypts the payload. It exists only inside
//!   this file, wrapped by the KEK — so the same container works for both
//!   modes and re-keying (PIN change) only ever needs the 60-byte wrap
//!   redone, never a payload re-encrypt.
//! - **Local mode** KEK = Argon2id(passphrase, random 16-byte salt) with the
//!   exact SVR parameters (128 MiB, t=8, p=4 — see `svr::get_argon2_params`);
//!   the passphrase must clear the same entropy floor as a PIN.
//! - **Server mode** KEK = HKDF-SHA256(ikm = svr_secret,
//!   info = "Chatter Message Backup KEK v1"). The server never sees the PIN,
//!   the svr_secret, or the KEK — the blob it stores is noise to it.
//! - Server index = HMAC-SHA256(svr_secret, "MessageBackupId"), mirroring
//!   `svr_backup::derive_backup_id`'s "BackupId" derivation with a distinct
//!   context string.
//!
//! Single-shot AES-GCM over the payload is deliberate: history backups are
//! text-dominated (MBs, not GBs) and GCM is safe far past that (~64 GiB per
//! key/nonce). Revisit chunking only if backups ever approach that.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use prost::Message;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::error::CryptoError;

/// File magic: identifies a chatter message-history backup, version 1 framing.
pub const MAGIC: &[u8; 8] = b"CHTRBKP1";

/// Container format version (inside the header, for in-place evolution).
pub const FORMAT_VERSION: u32 = 1;

/// KEK derived from a user passphrase (local file export).
pub const KEY_MODE_PASSPHRASE: u32 = 1;
/// KEK derived from the SVR secret (server-stored backup).
pub const KEY_MODE_SVR: u32 = 2;
/// KEK derived from a single-use Link'n'Sync ephemeral key (§30). The archive
/// is a device-to-device transfer that transits the CDN as ciphertext; the key
/// rode inside the E2E provision envelope, so — like SVR mode — there is no
/// Argon2 salt (the ephemeral key is already 32 bytes of fresh entropy).
pub const KEY_MODE_LINK: u32 = 3;

/// Argon2 salt length for passphrase mode.
pub const ARGON2_SALT_LEN: usize = 16;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// wrap nonce (12) || wrapped 32-byte file key || GCM tag (16)
const WRAPPED_FILE_KEY_LEN: usize = NONCE_LEN + 32 + TAG_LEN;

/// Cleartext container header. Everything needed to re-derive the KEK and
/// unwrap the file key; authenticated as AAD of the payload encryption.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BackupFileHeader {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    /// One of [`KEY_MODE_PASSPHRASE`] / [`KEY_MODE_SVR`] / [`KEY_MODE_LINK`].
    #[prost(uint32, tag = "2")]
    pub key_mode: u32,
    /// Random Argon2 salt (passphrase mode only; empty in SVR mode).
    #[prost(bytes = "vec", tag = "3")]
    pub argon2_salt: Vec<u8>,
    /// AES-256-GCM wrap of the random file key under the KEK:
    /// nonce(12) || ct(32) || tag(16).
    #[prost(bytes = "vec", tag = "4")]
    pub wrapped_file_key: Vec<u8>,
    /// Nonce for the payload encryption under the file key.
    #[prost(bytes = "vec", tag = "5")]
    pub payload_nonce: Vec<u8>,
}

/// Derive the local-mode KEK from a user passphrase.
///
/// Same Argon2id work factor as the SVR PIN derivation — the passphrase is
/// this file's only defense once it leaves the device, exactly the SVR
/// threat model. Callers must enforce `svr::validate_pin` on the passphrase
/// first (same entropy floor).
pub fn derive_passphrase_kek(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], CryptoError> {
    if salt.len() != ARGON2_SALT_LEN {
        return Err(CryptoError::Validation(format!(
            "backup salt must be {ARGON2_SALT_LEN} bytes, got {}",
            salt.len()
        )));
    }
    let argon2 = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        crate::crypto::svr::get_argon2_params(),
    );
    let mut out = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| CryptoError::KeyDerivationFailed(e.to_string()))?;
    Ok(out)
}

/// Derive the server-mode KEK from the SVR secret.
///
/// Distinct HKDF info string keeps this key domain-separated from every other
/// svr_secret consumer (key backup AES key, registration lock, backup ids).
pub fn derive_server_kek(svr_secret: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, svr_secret);
    let mut kek = [0u8; 32];
    hk.expand(b"Chatter Message Backup KEK v1", &mut kek)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    kek
}

/// Derive the Link'n'Sync archive KEK from the single-use ephemeral key that
/// travelled inside the E2E provision envelope (§30).
///
/// `HKDF-SHA256(ikm = ephemeral_key, salt = aci, info = "Chatter Link Sync KEK
/// v1|" || primary_registration_id)`.
///
/// Three bindings, in order of what they buy:
/// - **ephemeral key** — the actual secret. Single-use and 32 bytes of fresh
///   entropy per link, so a KEK can never be reused across sessions.
/// - **ACI salt** — binds the archive to the account. A blob stolen from the
///   CDN is undecryptable and cannot be replayed against another account, even
///   by someone who somehow learned an ephemeral key.
/// - **primary registration id in the info** — costless domain separation
///   (both sides already hold it: the primary knows its own, the new device
///   reads `primary_device_registration_id` from the same provision envelope
///   the ephemeral key came in). Binds the KEK to the specific primary that
///   created the link. Defense in depth, not load-bearing on its own.
///
/// Distinct info string from `derive_server_kek` keeps this domain-separated
/// from every other KEK.
pub fn derive_link_sync_kek(
    ephemeral_key: &[u8; 32],
    aci: &str,
    primary_registration_id: u32,
) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(aci.as_bytes()), ephemeral_key);
    let mut info = Vec::with_capacity(26 + 4);
    info.extend_from_slice(b"Chatter Link Sync KEK v1|");
    info.extend_from_slice(&primary_registration_id.to_be_bytes());
    let mut kek = [0u8; 32];
    hk.expand(&info, &mut kek)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    kek
}

/// Derive the server-side index for the message backup:
/// HMAC-SHA256(svr_secret, "MessageBackupId"). Mirrors
/// `svr_backup::derive_backup_id` ("BackupId") with a distinct context so the
/// two backups are independently addressable.
pub fn derive_message_backup_id(svr_secret: &[u8; 32]) -> [u8; 32] {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(svr_secret).expect("HMAC accepts any key size");
    mac.update(b"MessageBackupId");
    let mut id = [0u8; 32];
    id.copy_from_slice(&mac.finalize().into_bytes());
    id
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    use rand::TryRngCore;
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OS CSPRNG must not fail");
    buf
}

/// Encrypt a payload (length-delimited frame stream) into a complete backup
/// file. `argon2_salt` must be `ARGON2_SALT_LEN` bytes in passphrase mode and
/// empty in SVR mode.
pub fn encrypt_backup_file(
    payload: &[u8],
    kek: &[u8; 32],
    key_mode: u32,
    argon2_salt: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    match key_mode {
        KEY_MODE_PASSPHRASE if argon2_salt.len() == ARGON2_SALT_LEN => {}
        KEY_MODE_SVR if argon2_salt.is_empty() => {}
        // Link'n'Sync mode carries no Argon2 salt — the ephemeral key is the
        // entropy source (§30), same shape as SVR mode.
        KEY_MODE_LINK if argon2_salt.is_empty() => {}
        _ => {
            return Err(CryptoError::Validation(
                "invalid backup key mode / salt combination".into(),
            ));
        }
    }

    // Random file key, wrapped under the KEK. Skipping this wrap (encrypting
    // the payload directly under the KEK) would still round-trip — the unit
    // tests pin the wrap's presence via the header shape and the mutation
    // check in validated.rs::v50.
    let mut file_key = random_bytes::<32>();
    let wrap_nonce = random_bytes::<NONCE_LEN>();
    let kek_cipher = Aes256Gcm::new_from_slice(kek).map_err(|_| CryptoError::InvalidKeyFormat)?;
    let wrapped_ct = kek_cipher
        .encrypt(Nonce::from_slice(&wrap_nonce), file_key.as_ref())
        .map_err(|e| CryptoError::EncryptionFailed(format!("file-key wrap failed: {e}")))?;
    let mut wrapped_file_key = Vec::with_capacity(WRAPPED_FILE_KEY_LEN);
    wrapped_file_key.extend_from_slice(&wrap_nonce);
    wrapped_file_key.extend_from_slice(&wrapped_ct);

    let payload_nonce = random_bytes::<NONCE_LEN>();
    let header = BackupFileHeader {
        format_version: FORMAT_VERSION,
        key_mode,
        argon2_salt: argon2_salt.to_vec(),
        wrapped_file_key,
        payload_nonce: payload_nonce.to_vec(),
    };
    let header_bytes = header.encode_to_vec();

    let file_cipher =
        Aes256Gcm::new_from_slice(&file_key).map_err(|_| CryptoError::InvalidKeyFormat)?;
    file_key.zeroize();
    let ciphertext = file_cipher
        .encrypt(
            Nonce::from_slice(&payload_nonce),
            Payload {
                msg: payload,
                aad: &header_bytes,
            },
        )
        .map_err(|e| CryptoError::EncryptionFailed(format!("payload encrypt failed: {e}")))?;

    let mut out = Vec::with_capacity(MAGIC.len() + 4 + header_bytes.len() + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// A parsed (but not yet decrypted) backup file. The header is readable
/// without any key so the caller can derive the right KEK (mode + salt).
pub struct ParsedBackupFile<'a> {
    pub header: BackupFileHeader,
    header_bytes: &'a [u8],
    ciphertext: &'a [u8],
}

/// Parse the cleartext container structure. Fails on wrong magic, truncation,
/// or an unsupported format version.
pub fn parse_backup_file(bytes: &[u8]) -> Result<ParsedBackupFile<'_>, CryptoError> {
    if bytes.len() < MAGIC.len() + 4 {
        return Err(CryptoError::Validation("backup file truncated".into()));
    }
    let (magic, rest) = bytes.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(CryptoError::Validation(
            "not a chatter message backup (bad magic)".into(),
        ));
    }
    let (len_bytes, rest) = rest.split_at(4);
    let header_len =
        u32::from_le_bytes(len_bytes.try_into().expect("split_at(4) yields 4 bytes")) as usize;
    if rest.len() < header_len {
        return Err(CryptoError::Validation("backup header truncated".into()));
    }
    let (header_bytes, ciphertext) = rest.split_at(header_len);
    let header = BackupFileHeader::decode(header_bytes)
        .map_err(|e| CryptoError::Validation(format!("backup header undecodable: {e}")))?;
    if header.format_version != FORMAT_VERSION {
        return Err(CryptoError::Validation(format!(
            "unsupported backup format version {}",
            header.format_version
        )));
    }
    if header.payload_nonce.len() != NONCE_LEN
        || header.wrapped_file_key.len() != WRAPPED_FILE_KEY_LEN
    {
        return Err(CryptoError::Validation("backup header malformed".into()));
    }
    Ok(ParsedBackupFile {
        header,
        header_bytes,
        ciphertext,
    })
}

/// Unwrap the file key with the KEK and decrypt the payload. Any wrong key,
/// bit-flip in the ciphertext, or tamper with the header (it is the AAD)
/// fails the GCM tag — wrong passphrase / wrong PIN surfaces here.
pub fn decrypt_backup_file(
    parsed: &ParsedBackupFile<'_>,
    kek: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let (wrap_nonce, wrapped_ct) = parsed.header.wrapped_file_key.split_at(NONCE_LEN);
    let kek_cipher = Aes256Gcm::new_from_slice(kek).map_err(|_| CryptoError::InvalidKeyFormat)?;
    let mut file_key_vec = kek_cipher
        .decrypt(Nonce::from_slice(wrap_nonce), wrapped_ct)
        .map_err(|_| {
            CryptoError::DecryptionFailed("file-key unwrap failed (wrong passphrase/PIN?)".into())
        })?;
    if file_key_vec.len() != 32 {
        file_key_vec.zeroize();
        return Err(CryptoError::DecryptionFailed(
            "unwrapped file key has wrong length".into(),
        ));
    }

    let file_cipher =
        Aes256Gcm::new_from_slice(&file_key_vec).map_err(|_| CryptoError::InvalidKeyFormat)?;
    file_key_vec.zeroize();
    file_cipher
        .decrypt(
            Nonce::from_slice(&parsed.header.payload_nonce),
            Payload {
                msg: parsed.ciphertext,
                aad: parsed.header_bytes,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed("backup payload decrypt failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> [u8; 32] {
        core::array::from_fn(|i| i as u8)
    }

    /// Pinned derivation vectors (computed independently with Python
    /// hmac/hashlib HKDF). A silent change to the info/context strings or the
    /// KDF construction re-keys every existing backup — fail loudly instead.
    #[test]
    fn server_kek_and_backup_id_match_pinned_vectors() {
        assert_eq!(
            hex::encode(derive_server_kek(&secret())),
            "0a7f32256fb338f38b2751511dc417d04c4c16657bf3ffeb0c9d30a2049f9a4f",
        );
        assert_eq!(
            hex::encode(derive_message_backup_id(&secret())),
            "11192d4389f60baf9f35d5fd2d18fd7f76e0578c0c046e3223ba7fa44fa81de0",
        );
        // Distinct from the SVR key-backup id (HKDF info "Chatter SVR Backup Id
        // v1" since CRYPTO-3; it was HMAC(secret, "BackupId")) — the two backups
        // must be independently addressable server-side.
        assert_eq!(
            hex::encode(crate::crypto::svr_backup::derive_backup_id(&secret())),
            "c795c10a785300f313f43403a419adc4db433c5bf8148675c030aea76fbce02c",
        );
        assert_ne!(
            derive_message_backup_id(&secret()),
            crate::crypto::svr_backup::derive_backup_id(&secret())
        );
        // And the KEK must not equal the raw secret (domain separation).
        assert_ne!(derive_server_kek(&secret()), secret());
    }

    /// Link'n'Sync KEK (§30): deterministic per (ephemeral, aci, reg_id) and
    /// distinct when ANY of the three bindings changes. Pinned against an
    /// independently-computed HKDF vector so a silent change to the salt/info
    /// construction — which would make every in-flight transfer archive
    /// undecryptable — fails loudly. MUTATION: dropping the ACI salt or the
    /// reg-id from the derivation makes the corresponding assertion fail.
    #[test]
    fn link_sync_kek_binds_ephemeral_aci_and_regid() {
        let eph = secret();
        let aci = "11111111-1111-1111-1111-111111111111";
        let k = derive_link_sync_kek(&eph, aci, 7);

        // Deterministic.
        assert_eq!(k, derive_link_sync_kek(&eph, aci, 7));
        // Pinned: HKDF-SHA256(ikm=0..31, salt=aci bytes,
        // info="Chatter Link Sync KEK v1|" || 0x00000007), 32 bytes.
        assert_eq!(
            hex::encode(k),
            "dee2e015eeba8ef7f1f71c8599edfd13a64b08cd8709d7b0e685a1c4fe5fec7a",
        );

        // Each binding is load-bearing.
        assert_ne!(
            k,
            derive_link_sync_kek(&[9u8; 32], aci, 7),
            "ephemeral key binds"
        );
        assert_ne!(
            k,
            derive_link_sync_kek(&eph, "22222222-2222-2222-2222-222222222222", 7),
            "ACI salt binds — a blob cannot be replayed across accounts"
        );
        assert_ne!(
            k,
            derive_link_sync_kek(&eph, aci, 8),
            "primary reg-id binds"
        );
        // Domain separation from the SVR KEK built on the same 32 bytes.
        assert_ne!(k, derive_server_kek(&eph));
    }

    /// Full container round-trip in link mode: encrypt a frame payload under a
    /// link-sync KEK, parse, decrypt byte-exact. The wrong KEK (a mutated
    /// ephemeral key) fails to unwrap, and a header-tamper fails the AAD bind —
    /// the "MAC-verify before a single frame is imported" property §30 requires.
    #[test]
    fn link_mode_container_round_trips_and_rejects_wrong_key() {
        let aci = "abcdefab-cdef-abcd-efab-cdefabcdefab";
        let kek = derive_link_sync_kek(&secret(), aci, 1);
        let payload = b"length-delimited archive frames".to_vec();

        let file = encrypt_backup_file(&payload, &kek, KEY_MODE_LINK, &[]).unwrap();
        let parsed = parse_backup_file(&file).unwrap();
        assert_eq!(parsed.header.key_mode, KEY_MODE_LINK);
        assert_eq!(decrypt_backup_file(&parsed, &kek).unwrap(), payload);

        // Wrong ephemeral key (attacker who has the blob but not the envelope).
        let wrong = derive_link_sync_kek(&[0xFFu8; 32], aci, 1);
        assert!(decrypt_backup_file(&parsed, &wrong).is_err());

        // A link-mode file must carry no Argon2 salt.
        assert!(
            encrypt_backup_file(&payload, &kek, KEY_MODE_LINK, &[0u8; ARGON2_SALT_LEN]).is_err()
        );

        // Header tamper (flip a byte in the header) breaks the AAD bind.
        let mut tampered = file.clone();
        tampered[MAGIC.len() + 4] ^= 0x01;
        assert!(
            parse_backup_file(&tampered)
                .and_then(|p| decrypt_backup_file(&p, &kek))
                .is_err()
        );
    }

    /// Passphrase KEK: deterministic for same (passphrase, salt), distinct
    /// across salts and passphrases. MUTATION: dropping the salt from the
    /// Argon2 input would make the second assertion fail.
    #[test]
    fn passphrase_kek_is_salted_and_deterministic() {
        let salt_a = [7u8; ARGON2_SALT_LEN];
        let salt_b = [8u8; ARGON2_SALT_LEN];
        let a1 = derive_passphrase_kek("correct horse battery", &salt_a).unwrap();
        let a2 = derive_passphrase_kek("correct horse battery", &salt_a).unwrap();
        let b = derive_passphrase_kek("correct horse battery", &salt_b).unwrap();
        let c = derive_passphrase_kek("wrong horse battery", &salt_a).unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b, "different salt must derive a different KEK");
        assert_ne!(a1, c, "different passphrase must derive a different KEK");
        // Wrong salt length is a validation error, not a silent truncation.
        assert!(derive_passphrase_kek("x", &[0u8; 8]).is_err());
    }

    /// Container round-trip + the negative halves: wrong KEK fails, header
    /// tamper fails (AAD binding), truncation fails. MUTATION: removing the
    /// `aad:` argument from encrypt/decrypt makes the tamper assertion fail.
    #[test]
    fn container_round_trips_and_rejects_tampering() {
        let kek = derive_server_kek(&secret());
        let payload = b"length-delimited frames would live here".to_vec();
        let file = encrypt_backup_file(&payload, &kek, KEY_MODE_SVR, &[]).unwrap();

        // Round trip
        let parsed = parse_backup_file(&file).unwrap();
        assert_eq!(parsed.header.key_mode, KEY_MODE_SVR);
        assert_eq!(decrypt_backup_file(&parsed, &kek).unwrap(), payload);

        // Ciphertext must not contain the plaintext (sanity for the
        // no-plaintext greps in V50/V51)
        assert!(!file.windows(payload.len()).any(|w| w == payload.as_slice()));

        // Wrong KEK
        let wrong = derive_server_kek(&[9u8; 32]);
        assert!(decrypt_backup_file(&parsed, &wrong).is_err());

        // Header tamper: flip a byte inside the header region — GCM AAD check
        // must reject even though the payload ciphertext is untouched.
        let mut tampered = file.clone();
        tampered[MAGIC.len() + 4] ^= 0x01;
        match parse_backup_file(&tampered) {
            Ok(p) => assert!(decrypt_backup_file(&p, &kek).is_err()),
            Err(_) => {} // header may also fail to decode — equally fatal
        }

        // Ciphertext truncation
        let short = &file[..file.len() - 1];
        let parsed_short = parse_backup_file(short).unwrap();
        assert!(decrypt_backup_file(&parsed_short, &kek).is_err());

        // Bad magic
        let mut bad = file.clone();
        bad[0] ^= 0xFF;
        assert!(parse_backup_file(&bad).is_err());
    }

    /// Mode/salt agreement is enforced at encrypt time.
    #[test]
    fn encrypt_rejects_mode_salt_mismatch() {
        let kek = [1u8; 32];
        assert!(encrypt_backup_file(b"x", &kek, KEY_MODE_PASSPHRASE, &[]).is_err());
        assert!(encrypt_backup_file(b"x", &kek, KEY_MODE_SVR, &[0u8; ARGON2_SALT_LEN]).is_err());
        assert!(encrypt_backup_file(b"x", &kek, 99, &[]).is_err());
    }
}
